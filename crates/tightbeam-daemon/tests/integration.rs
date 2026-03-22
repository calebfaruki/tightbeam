use async_trait::async_trait;
use futures::stream;
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tightbeam_daemon::lifecycle;
use tightbeam_daemon::profile::{AgentProfile, LogConfig, ProviderConfig, SocketConfig};
use tightbeam_daemon::protocol::{Message, ToolDefinition};
use tightbeam_daemon::provider::{self, LlmProvider, StreamEvent};
use tightbeam_daemon::{bind_agent_socket, run_daemon, ConversationMap, ProfileMap, ProviderMap};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::RwLock;

// --- MockProvider ---

struct CapturedCall {
    #[allow(dead_code)]
    messages: Vec<Message>,
    system: Option<String>,
    tools: Vec<ToolDefinition>,
}

struct MockProvider {
    responses: Arc<Mutex<VecDeque<Vec<StreamEvent>>>>,
    call_log: Arc<Mutex<Vec<CapturedCall>>>,
}

impl MockProvider {
    fn new(responses: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(VecDeque::from(responses))),
            call_log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn call_log(&self) -> Arc<Mutex<Vec<CapturedCall>>> {
        self.call_log.clone()
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    async fn call(
        &self,
        messages: &[Message],
        system: Option<&str>,
        tools: &[ToolDefinition],
        _config: &provider::ProviderConfig,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<StreamEvent, String>> + Send>>, String>
    {
        self.call_log.lock().unwrap().push(CapturedCall {
            messages: messages.to_vec(),
            system: system.map(|s| s.to_string()),
            tools: tools.to_vec(),
        });

        let events = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| "MockProvider: no more responses queued".to_string())?;

        let event_stream = stream::iter(events.into_iter().map(Ok));
        Ok(Box::pin(event_stream))
    }
}

// --- Test infrastructure ---

fn test_socket_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tb-{}-{}.sock", name, std::process::id()))
}

fn test_logs_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tb-logs-{}-{}", name, std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn make_profile() -> AgentProfile {
    AgentProfile {
        provider: ProviderConfig {
            name: "mock".into(),
            model: "test-model".into(),
            api_key_env: "TIGHTBEAM_TEST_KEY".into(),
            max_tokens: 1024,
        },
        socket: SocketConfig {
            path: "test.sock".into(),
        },
        log: LogConfig {
            path: "test-agent/".into(),
        },
    }
}

async fn start_daemon(
    sock_path: &std::path::Path,
    provider: MockProvider,
    logs_dir: PathBuf,
) -> tokio::task::JoinHandle<()> {
    let mut profiles = HashMap::new();
    profiles.insert("test-agent".to_string(), make_profile());
    let profiles: ProfileMap = Arc::new(profiles);

    let conversations: ConversationMap = Arc::new(RwLock::new(HashMap::new()));

    let mut providers: HashMap<String, Box<dyn LlmProvider>> = HashMap::new();
    providers.insert("mock".into(), Box::new(provider));
    let providers: ProviderMap = Arc::new(providers);

    let idle_map = lifecycle::new_idle_map();

    let listener = bind_agent_socket(sock_path).unwrap();
    let listeners = vec![("test-agent".to_string(), listener)];

    tokio::spawn(async move {
        run_daemon(
            listeners,
            profiles,
            conversations,
            providers,
            idle_map,
            logs_dir,
        )
        .await;
    })
}

async fn send_and_collect(sock_path: &std::path::Path, request_json: &str) -> Vec<String> {
    let mut stream = UnixStream::connect(sock_path).await.unwrap();
    stream
        .write_all(format!("{request_json}\n").as_bytes())
        .await
        .unwrap();
    stream.shutdown().await.unwrap();

    let mut reader = BufReader::new(stream);
    let mut lines = Vec::new();
    let mut buf = String::new();
    while reader.read_line(&mut buf).await.unwrap() > 0 {
        lines.push(buf.trim().to_string());
        buf.clear();
    }
    lines
}

async fn send_and_read_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    request_json: &str,
) -> Vec<String> {
    writer
        .write_all(format!("{request_json}\n").as_bytes())
        .await
        .unwrap();
    writer.flush().await.unwrap();

    let mut lines = Vec::new();
    let mut buf = String::new();
    loop {
        buf.clear();
        reader.read_line(&mut buf).await.unwrap();
        let trimmed = buf.trim().to_string();
        if trimmed.is_empty() {
            continue;
        }
        let has_id = serde_json::from_str::<serde_json::Value>(&trimmed)
            .ok()
            .and_then(|v| v.get("id").cloned())
            .is_some();
        lines.push(trimmed);
        if has_id {
            break;
        }
    }
    lines
}

#[derive(Deserialize)]
struct AnyResponse {
    id: Option<u64>,
    method: Option<String>,
    result: Option<serde_json::Value>,
    error: Option<serde_json::Value>,
    #[serde(rename = "params")]
    _params: Option<serde_json::Value>,
}

fn assert_error_contains(lines: &[String], expected: &str) {
    assert!(!lines.is_empty(), "expected at least one response line");
    let resp: AnyResponse = serde_json::from_str(&lines[0]).unwrap();
    assert!(resp.error.is_some(), "expected error response");
    let message = resp.error.unwrap()["message"].as_str().unwrap().to_string();
    assert!(
        message.contains(expected),
        "expected '{expected}' in: {message}"
    );
}

fn find_final_response(lines: &[String]) -> AnyResponse {
    for line in lines {
        let resp: AnyResponse = serde_json::from_str(line).unwrap();
        if resp.id.is_some() {
            return resp;
        }
    }
    panic!("no final response (with id) found in: {lines:?}");
}

fn count_notifications(lines: &[String]) -> usize {
    lines
        .iter()
        .filter(|l| {
            serde_json::from_str::<AnyResponse>(l)
                .ok()
                .map(|r| r.method.is_some() && r.id.is_none())
                .unwrap_or(false)
        })
        .count()
}

const WAIT: std::time::Duration = std::time::Duration::from_millis(50);

// --- Tests ---

mod protocol_integration {
    use super::*;

    #[tokio::test]
    async fn llm_call_returns_streaming_notifications_and_final_response() {
        let sock = test_socket_path("stream");
        let logs = test_logs_dir("stream");

        let provider = MockProvider::new(vec![vec![
            StreamEvent::ContentDelta {
                text: "Hello ".into(),
            },
            StreamEvent::ContentDelta {
                text: "world".into(),
            },
            StreamEvent::Done {
                stop_reason: "end_turn".into(),
            },
        ]]);

        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        let request = r#"{"jsonrpc":"2.0","id":1,"method":"llm_call","params":{"messages":[{"role":"user","content":"Hi"}],"tools":[]}}"#;
        let lines = send_and_collect(&sock, request).await;

        let notif_count = count_notifications(&lines);
        assert!(
            notif_count >= 2,
            "expected at least 2 notifications, got {notif_count}"
        );

        let final_resp = find_final_response(&lines);
        assert_eq!(final_resp.id, Some(1));
        let result = final_resp.result.unwrap();
        assert_eq!(result["stop_reason"], "end_turn");
        assert_eq!(result["text"], "Hello world");
        assert!(result.get("tool_calls").is_none());

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn llm_call_with_tool_use_returns_tool_calls() {
        let sock = test_socket_path("tooluse");
        let logs = test_logs_dir("tooluse");

        let provider = MockProvider::new(vec![vec![
            StreamEvent::ToolUseStart {
                id: "tc-1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolUseInput {
                json: r#"{"command":"ls"}"#.into(),
            },
            StreamEvent::Done {
                stop_reason: "tool_use".into(),
            },
        ]]);

        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        let request = r#"{"jsonrpc":"2.0","id":1,"method":"llm_call","params":{"messages":[{"role":"user","content":"List files"}],"tools":[]}}"#;
        let lines = send_and_collect(&sock, request).await;

        let final_resp = find_final_response(&lines);
        let result = final_resp.result.unwrap();
        assert_eq!(result["stop_reason"], "tool_use");

        let tool_calls = result["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "tc-1");
        assert_eq!(tool_calls[0]["name"], "bash");
        assert_eq!(tool_calls[0]["input"]["command"], "ls");
        assert!(result.get("text").is_none());

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn tool_result_triggers_continuation_with_cached_tools() {
        let sock = test_socket_path("toolcont");
        let logs = test_logs_dir("toolcont");

        let provider = MockProvider::new(vec![
            // Response to llm_call
            vec![
                StreamEvent::ToolUseStart {
                    id: "tc-1".into(),
                    name: "bash".into(),
                },
                StreamEvent::ToolUseInput {
                    json: r#"{"command":"ls"}"#.into(),
                },
                StreamEvent::Done {
                    stop_reason: "tool_use".into(),
                },
            ],
            // Response to tool_result continuation
            vec![
                StreamEvent::ContentDelta {
                    text: "Files: main.rs".into(),
                },
                StreamEvent::Done {
                    stop_reason: "end_turn".into(),
                },
            ],
        ]);

        let call_log = provider.call_log();
        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        // Single persistent connection for the full tool loop
        let stream = UnixStream::connect(&sock).await.unwrap();
        let (read_half, write_half) = stream.into_split();
        let mut writer = write_half;
        let mut reader = BufReader::new(read_half);

        // Step 1: llm_call with tools defined
        let request1 = r#"{"jsonrpc":"2.0","id":1,"method":"llm_call","params":{"messages":[{"role":"user","content":"List files"}],"tools":[{"name":"bash","description":"Run a command","input_schema":{"type":"object"}}]}}"#;
        let lines1 = send_and_read_response(&mut writer, &mut reader, request1).await;
        let resp1 = find_final_response(&lines1);
        assert_eq!(resp1.result.as_ref().unwrap()["stop_reason"], "tool_use");

        // Step 2: tool_result on same connection
        let request2 = r#"{"jsonrpc":"2.0","id":2,"method":"tool_result","params":{"tool_call_id":"tc-1","result":"main.rs\nlib.rs\n"}}"#;
        let lines2 = send_and_read_response(&mut writer, &mut reader, request2).await;
        let resp2 = find_final_response(&lines2);
        let result2 = resp2.result.unwrap();
        assert_eq!(result2["stop_reason"], "end_turn");
        assert_eq!(result2["text"], "Files: main.rs");

        // Verify tools were cached and passed to the second provider call
        let log = call_log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert!(
            !log[1].tools.is_empty(),
            "second call should have cached tools, got empty"
        );
        assert_eq!(log[1].tools[0].name, "bash");

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn conversation_log_written_to_disk() {
        let sock = test_socket_path("convlog");
        let logs = test_logs_dir("convlog");

        let provider = MockProvider::new(vec![vec![
            StreamEvent::ContentDelta {
                text: "Hi there".into(),
            },
            StreamEvent::Done {
                stop_reason: "end_turn".into(),
            },
        ]]);

        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        let request = r#"{"jsonrpc":"2.0","id":1,"method":"llm_call","params":{"messages":[{"role":"user","content":"Hello"}],"tools":[]}}"#;
        let lines = send_and_collect(&sock, request).await;
        let _ = find_final_response(&lines); // ensure success

        // Small delay for file flush
        tokio::time::sleep(WAIT).await;

        let log_path = logs.join("test-agent/conversation.ndjson");
        assert!(log_path.exists(), "conversation log should exist");

        let content = std::fs::read_to_string(&log_path).unwrap();
        let entries: Vec<serde_json::Value> = content
            .trim()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        assert!(entries.len() >= 2, "expected at least 2 log entries");
        assert_eq!(entries[0]["role"], "user");
        assert_eq!(entries[1]["role"], "assistant");

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn invalid_json_returns_parse_error() {
        let sock = test_socket_path("badjson");
        let logs = test_logs_dir("badjson");

        let provider = MockProvider::new(vec![]);
        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        let lines = send_and_collect(&sock, "not json at all").await;
        assert_error_contains(&lines, "parse error");

        let resp: AnyResponse = serde_json::from_str(&lines[0]).unwrap();
        let code = resp.error.unwrap()["code"].as_i64().unwrap();
        assert_eq!(code, -32700);

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let sock = test_socket_path("unkmethod");
        let logs = test_logs_dir("unkmethod");

        let provider = MockProvider::new(vec![]);
        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        let request = r#"{"jsonrpc":"2.0","id":1,"method":"bogus","params":{}}"#;
        let lines = send_and_collect(&sock, request).await;
        assert_error_contains(&lines, "unknown method");

        let resp: AnyResponse = serde_json::from_str(&lines[0]).unwrap();
        let code = resp.error.unwrap()["code"].as_i64().unwrap();
        assert_eq!(code, -32601);

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn future_methods_return_not_implemented() {
        let sock = test_socket_path("future");
        let logs = test_logs_dir("future");

        let provider = MockProvider::new(vec![]);
        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        let send_msg =
            r#"{"jsonrpc":"2.0","id":1,"method":"send_message","params":{"text":"hello"}}"#;
        let lines = send_and_collect(&sock, send_msg).await;
        assert_error_contains(&lines, "not implemented");

        let mcp = r#"{"jsonrpc":"2.0","id":2,"method":"mcp_call","params":{"server":"github"}}"#;
        let lines = send_and_collect(&sock, mcp).await;
        assert_error_contains(&lines, "not implemented");

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn system_prompt_cached_on_first_call() {
        let sock = test_socket_path("sysprompt");
        let logs = test_logs_dir("sysprompt");

        let provider = MockProvider::new(vec![
            vec![
                StreamEvent::ContentDelta {
                    text: "First".into(),
                },
                StreamEvent::Done {
                    stop_reason: "end_turn".into(),
                },
            ],
            vec![
                StreamEvent::ContentDelta {
                    text: "Second".into(),
                },
                StreamEvent::Done {
                    stop_reason: "end_turn".into(),
                },
            ],
        ]);

        let call_log = provider.call_log();
        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        // Single connection for both calls
        let stream = UnixStream::connect(&sock).await.unwrap();
        let (read_half, write_half) = stream.into_split();
        let mut writer = write_half;
        let mut reader = BufReader::new(read_half);

        // First call with system prompt
        let req1 = r#"{"jsonrpc":"2.0","id":1,"method":"llm_call","params":{"messages":[{"role":"user","content":"Hi"}],"tools":[],"system":"You are helpful"}}"#;
        let _ = send_and_read_response(&mut writer, &mut reader, req1).await;

        // Second call with different system prompt (should be ignored)
        let req2 = r#"{"jsonrpc":"2.0","id":2,"method":"llm_call","params":{"messages":[{"role":"user","content":"Hello again"}],"tools":[],"system":"Ignored"}}"#;
        let _ = send_and_read_response(&mut writer, &mut reader, req2).await;

        let log = call_log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(
            log[0].system.as_deref(),
            Some("You are helpful"),
            "first call should have the system prompt"
        );
        assert_eq!(
            log[1].system.as_deref(),
            Some("You are helpful"),
            "second call should still have the first system prompt, not 'Ignored'"
        );

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }
}

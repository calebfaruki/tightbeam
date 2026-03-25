use async_trait::async_trait;
use futures::stream;
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tightbeam_daemon::mcp::McpManager;
use tightbeam_daemon::profile::{AgentConfig, ResolvedLlm, ResolvedMcp};
use tightbeam_daemon::protocol::{Message, ToolDefinition};
use tightbeam_daemon::{
    bind_agent_socket, run_daemon, ConversationMap, McpManagerMap, ProfileMap, ProviderMap,
};
use tightbeam_protocol::framing::{read_frame, write_frame};
use tightbeam_providers::{LlmProvider, ProviderConfig, StreamEvent};
use tokio::net::UnixStream;
use tokio::sync::RwLock;

// --- MockProvider ---

struct CapturedCall {
    system: Option<String>,
    tools: Vec<ToolDefinition>,
    messages: Vec<Message>,
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
        _config: &ProviderConfig,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<StreamEvent, String>> + Send>>, String>
    {
        self.call_log.lock().unwrap().push(CapturedCall {
            system: system.map(|s| s.to_string()),
            tools: tools.to_vec(),
            messages: messages.to_vec(),
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

fn make_profile() -> AgentConfig {
    AgentConfig {
        llm: ResolvedLlm {
            provider: tightbeam_providers::Provider::Anthropic,
            model: "test-model".into(),
            api_key: "test-key".into(),
            max_tokens: 1024,
        },
        mcp_servers: Vec::new(),
    }
}

async fn start_daemon(
    sock_path: &std::path::Path,
    provider: MockProvider,
    logs_dir: PathBuf,
) -> tokio::task::JoinHandle<()> {
    let mut profiles = HashMap::new();
    profiles.insert("test-agent".to_string(), make_profile());
    let profiles: ProfileMap = Arc::new(RwLock::new(profiles));

    let conversations: ConversationMap = Arc::new(RwLock::new(HashMap::new()));

    let mut providers: HashMap<tightbeam_providers::Provider, Box<dyn LlmProvider>> =
        HashMap::new();
    providers.insert(tightbeam_providers::Provider::Anthropic, Box::new(provider));
    let providers: ProviderMap = Arc::new(RwLock::new(providers));

    let mcp_managers: McpManagerMap = Arc::new(RwLock::new(HashMap::new()));

    let listener = bind_agent_socket(sock_path).unwrap();
    let listeners = vec![("test-agent".to_string(), listener)];

    let base = logs_dir.parent().unwrap_or(&logs_dir).to_path_buf();
    let sockets_dir = base.join("sockets");
    let agents_dir = base.join("agents");

    tokio::spawn(async move {
        run_daemon(
            listeners,
            profiles,
            conversations,
            providers,
            mcp_managers,
            logs_dir,
            sockets_dir,
            agents_dir,
        )
        .await;
    })
}

#[tokio::test]
async fn zero_agents_daemon_starts() {
    let profiles: ProfileMap = Arc::new(RwLock::new(HashMap::new()));
    let conversations: ConversationMap = Arc::new(RwLock::new(HashMap::new()));
    let providers: ProviderMap = Arc::new(RwLock::new(HashMap::new()));
    let mcp_managers: McpManagerMap = Arc::new(RwLock::new(HashMap::new()));

    let logs = test_logs_dir("zero-agents");
    let base = logs.parent().unwrap_or(&logs).to_path_buf();
    let sockets_dir = base.join("sockets");
    let agents_dir = base.join("agents");

    let handle = tokio::spawn(async move {
        run_daemon(
            vec![],
            profiles,
            conversations,
            providers,
            mcp_managers,
            logs,
            sockets_dir,
            agents_dir,
        )
        .await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !handle.is_finished(),
        "daemon should still be running with zero agents"
    );
    handle.abort();
    let _ = std::fs::remove_dir_all(test_logs_dir("zero-agents"));
}

const REGISTER_FRAME: &str = r#"{"jsonrpc":"2.0","method":"register","params":{"role":"runtime"}}"#;

async fn send_register(writer: &mut tokio::net::unix::OwnedWriteHalf) {
    write_frame(writer, REGISTER_FRAME.as_bytes())
        .await
        .unwrap();
}

async fn send_and_collect(sock_path: &std::path::Path, request_json: &str) -> Vec<String> {
    let stream = UnixStream::connect(sock_path).await.unwrap();
    let (mut reader, mut writer) = stream.into_split();

    send_register(&mut writer).await;
    write_frame(&mut writer, request_json.as_bytes())
        .await
        .unwrap();
    drop(writer);

    let mut lines = Vec::new();
    while let Ok(Some(frame)) = read_frame(&mut reader).await {
        lines.push(String::from_utf8_lossy(&frame).to_string());
    }
    lines
}

async fn send_and_read_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    reader: &mut tokio::net::unix::OwnedReadHalf,
    request_json: &str,
) -> Vec<String> {
    write_frame(writer, request_json.as_bytes()).await.unwrap();

    let mut lines = Vec::new();
    loop {
        let frame = read_frame(reader).await.unwrap().unwrap();
        let text = String::from_utf8_lossy(&frame).to_string();
        let has_id = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("id").cloned())
            .is_some();
        lines.push(text);
        if has_id {
            break;
        }
    }
    lines
}

async fn read_one_frame(reader: &mut tokio::net::unix::OwnedReadHalf) -> serde_json::Value {
    let frame = read_frame(reader).await.unwrap().unwrap();
    let raw = String::from_utf8_lossy(&frame);
    serde_json::from_str(&raw).unwrap()
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
    async fn turn_returns_streaming_notifications_and_final_response() {
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

        let request = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"messages":[{"role":"user","content":[{"type":"text","text":"Hi"}]}]}}"#;
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
        assert_eq!(result["content"][0]["text"], "Hello world");
        assert!(result.get("tool_calls").is_none());

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn turn_with_tool_use_returns_tool_calls() {
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

        let request = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"messages":[{"role":"user","content":[{"type":"text","text":"List files"}]}]}}"#;
        let lines = send_and_collect(&sock, request).await;

        let final_resp = find_final_response(&lines);
        let result = final_resp.result.unwrap();
        assert_eq!(result["stop_reason"], "tool_use");

        let tool_calls = result["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "tc-1");
        assert_eq!(tool_calls[0]["name"], "bash");
        assert_eq!(tool_calls[0]["input"]["command"], "ls");
        assert!(result.get("content").is_none());

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn tool_result_triggers_continuation_with_cached_tools() {
        let sock = test_socket_path("toolcont");
        let logs = test_logs_dir("toolcont");

        let provider = MockProvider::new(vec![
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

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (mut reader, mut writer) = stream.into_split();
        send_register(&mut writer).await;

        let request1 = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"system":"You are helpful","tools":[{"name":"bash","description":"Run a command","parameters":{"type":"object"}}],"messages":[{"role":"user","content":[{"type":"text","text":"List files"}]}]}}"#;
        let lines1 = send_and_read_response(&mut writer, &mut reader, request1).await;
        let resp1 = find_final_response(&lines1);
        assert_eq!(resp1.result.as_ref().unwrap()["stop_reason"], "tool_use");

        let request2 = r#"{"jsonrpc":"2.0","id":2,"method":"turn","params":{"messages":[{"role":"tool","tool_call_id":"tc-1","content":[{"type":"text","text":"main.rs\nlib.rs\n"}]}]}}"#;
        let lines2 = send_and_read_response(&mut writer, &mut reader, request2).await;
        let resp2 = find_final_response(&lines2);
        let result2 = resp2.result.unwrap();
        assert_eq!(result2["stop_reason"], "end_turn");
        assert_eq!(result2["content"][0]["text"], "Files: main.rs");

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

        let request = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"messages":[{"role":"user","content":[{"type":"text","text":"Hello"}]}]}}"#;
        let lines = send_and_collect(&sock, request).await;
        let _ = find_final_response(&lines);

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
    async fn mcp_call_returns_not_implemented() {
        let sock = test_socket_path("future");
        let logs = test_logs_dir("future");

        let provider = MockProvider::new(vec![]);
        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        let mcp = r#"{"jsonrpc":"2.0","id":2,"method":"mcp_call","params":{"server":"github"}}"#;
        let lines = send_and_collect(&sock, mcp).await;
        assert_error_contains(&lines, "not implemented");

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn system_prompt_cached_on_first_turn() {
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

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (mut reader, mut writer) = stream.into_split();
        send_register(&mut writer).await;

        let req1 = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"system":"You are helpful","messages":[{"role":"user","content":[{"type":"text","text":"Hi"}]}]}}"#;
        let _ = send_and_read_response(&mut writer, &mut reader, req1).await;

        let req2 = r#"{"jsonrpc":"2.0","id":2,"method":"turn","params":{"system":"Ignored","messages":[{"role":"user","content":[{"type":"text","text":"Hello again"}]}]}}"#;
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

    #[tokio::test]
    async fn turn_with_empty_messages_succeeds() {
        let sock = test_socket_path("emptymsg");
        let logs = test_logs_dir("emptymsg");

        let provider = MockProvider::new(vec![vec![
            StreamEvent::ContentDelta { text: "OK".into() },
            StreamEvent::Done {
                stop_reason: "end_turn".into(),
            },
        ]]);

        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        let request = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"messages":[]}}"#;
        let lines = send_and_collect(&sock, request).await;

        let final_resp = find_final_response(&lines);
        assert_eq!(final_resp.id, Some(1));
        let result = final_resp.result.unwrap();
        assert_eq!(result["stop_reason"], "end_turn");

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }
}

// --- MCP integration tests ---

mod mcp_integration {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn start_mock_mcp_server(
        responses: Vec<serde_json::Value>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");

        let resps = Arc::new(Mutex::new(VecDeque::from(responses)));

        let handle = tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };
                let resps = resps.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    let resp = resps
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or(serde_json::json!({"jsonrpc":"2.0","error":{"code":-1,"message":"no response"}}));
                    let body = serde_json::to_string(&resp).unwrap();
                    let http = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(http.as_bytes()).await;
                });
            }
        });

        (url, handle)
    }

    async fn start_daemon_with_mcp(
        sock_path: &std::path::Path,
        provider: MockProvider,
        logs_dir: PathBuf,
        mcp_configs: Vec<ResolvedMcp>,
    ) -> tokio::task::JoinHandle<()> {
        let mut profiles = HashMap::new();
        profiles.insert("test-agent".to_string(), make_profile());
        let profiles: ProfileMap = Arc::new(RwLock::new(profiles));

        let conversations: ConversationMap = Arc::new(RwLock::new(HashMap::new()));

        let mut providers: HashMap<tightbeam_providers::Provider, Box<dyn LlmProvider>> =
            HashMap::new();
        providers.insert(tightbeam_providers::Provider::Anthropic, Box::new(provider));
        let providers: ProviderMap = Arc::new(RwLock::new(providers));

        let mut mcp_map = HashMap::new();
        mcp_map.insert("test-agent".to_string(), McpManager::new(mcp_configs));
        let mcp_managers: McpManagerMap = Arc::new(RwLock::new(mcp_map));

        let listener = bind_agent_socket(sock_path).unwrap();
        let listeners = vec![("test-agent".to_string(), listener)];

        let base = logs_dir.parent().unwrap_or(&logs_dir).to_path_buf();
        let sockets_dir = base.join("sockets");
        let agents_dir = base.join("agents");

        tokio::spawn(async move {
            run_daemon(
                listeners,
                profiles,
                conversations,
                providers,
                mcp_managers,
                logs_dir,
                sockets_dir,
                agents_dir,
            )
            .await;
        })
    }

    #[tokio::test]
    async fn no_mcp_servers_behaves_identically() {
        let sock = test_socket_path("nomcp");
        let logs = test_logs_dir("nomcp");

        let provider = MockProvider::new(vec![vec![
            StreamEvent::ContentDelta {
                text: "Hello".into(),
            },
            StreamEvent::Done {
                stop_reason: "end_turn".into(),
            },
        ]]);

        let _handle = start_daemon_with_mcp(&sock, provider, logs.clone(), vec![]).await;
        tokio::time::sleep(WAIT).await;

        let request = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"messages":[{"role":"user","content":[{"type":"text","text":"Hi"}]}]}}"#;
        let lines = send_and_collect(&sock, request).await;

        let final_resp = find_final_response(&lines);
        assert_eq!(final_resp.result.unwrap()["content"][0]["text"], "Hello");

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn all_mcp_loop_daemon_loops_internally() {
        let sock = test_socket_path("allmcp");
        let logs = test_logs_dir("allmcp");

        let tools_list = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"tools": [
                {"name": "mcp_search", "description": "Search", "inputSchema": {"type": "object"}}
            ]}
        });
        let tool_result = serde_json::json!({
            "jsonrpc": "2.0", "id": 2,
            "result": {"content": [{"type": "text", "text": "search result: found it"}]}
        });
        let (mcp_url, _mcp_handle) = start_mock_mcp_server(vec![tools_list, tool_result]).await;

        let mcp_configs = vec![ResolvedMcp {
            name: "search".into(),
            url: mcp_url,
            auth_token: "test-token".into(),
            tool_allowlist: None,
        }];

        let provider = MockProvider::new(vec![
            vec![
                StreamEvent::ToolUseStart {
                    id: "tc-mcp-1".into(),
                    name: "mcp_search".into(),
                },
                StreamEvent::ToolUseInput {
                    json: r#"{"query":"rust"}"#.into(),
                },
                StreamEvent::Done {
                    stop_reason: "tool_use".into(),
                },
            ],
            vec![
                StreamEvent::ContentDelta {
                    text: "Found: rust".into(),
                },
                StreamEvent::Done {
                    stop_reason: "end_turn".into(),
                },
            ],
        ]);

        let call_log = provider.call_log();
        let _handle = start_daemon_with_mcp(&sock, provider, logs.clone(), mcp_configs).await;
        tokio::time::sleep(WAIT).await;

        let request = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"tools":[{"name":"bash","description":"Run","parameters":{"type":"object"}}],"messages":[{"role":"user","content":[{"type":"text","text":"Search for rust"}]}]}}"#;
        let lines = send_and_collect(&sock, request).await;

        let final_resp = find_final_response(&lines);
        let result = final_resp.result.unwrap();
        assert_eq!(result["stop_reason"], "end_turn");
        assert_eq!(result["content"][0]["text"], "Found: rust");

        let log = call_log.lock().unwrap();
        assert_eq!(log.len(), 2);
        let tool_names: Vec<&str> = log[0].tools.iter().map(|t| t.name.as_str()).collect();
        assert!(tool_names.contains(&"bash"), "should see local tool");
        assert!(tool_names.contains(&"mcp_search"), "should see MCP tool");

        let second_call_msgs = &log[1].messages;
        let has_mcp_result = second_call_msgs.iter().any(|m| {
            m.role == "tool"
                && m.tool_call_id.as_deref() == Some("tc-mcp-1")
                && tightbeam_protocol::content_text(&m.content)
                    .unwrap_or("")
                    .contains("search result: found it")
        });
        assert!(
            has_mcp_result,
            "second LLM call should see MCP tool result in conversation. Messages: {:?}",
            second_call_msgs
                .iter()
                .map(|m| (&m.role, &m.tool_call_id))
                .collect::<Vec<_>>()
        );

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn mixed_mcp_and_local_tool_calls() {
        let sock = test_socket_path("mixedmcp");
        let logs = test_logs_dir("mixedmcp");

        let tools_list = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"tools": [
                {"name": "mcp_search", "description": "Search", "inputSchema": {"type": "object"}}
            ]}
        });
        let tool_result = serde_json::json!({
            "jsonrpc": "2.0", "id": 2,
            "result": {"content": [{"type": "text", "text": "mcp result"}]}
        });
        let (mcp_url, _mcp_handle) = start_mock_mcp_server(vec![tools_list, tool_result]).await;

        let mcp_configs = vec![ResolvedMcp {
            name: "search".into(),
            url: mcp_url,
            auth_token: "test-token".into(),
            tool_allowlist: None,
        }];

        let provider = MockProvider::new(vec![
            vec![
                StreamEvent::ToolUseStart {
                    id: "tc-mcp".into(),
                    name: "mcp_search".into(),
                },
                StreamEvent::ToolUseInput {
                    json: r#"{"q":"test"}"#.into(),
                },
                StreamEvent::ToolUseStart {
                    id: "tc-local".into(),
                    name: "bash".into(),
                },
                StreamEvent::ToolUseInput {
                    json: r#"{"command":"ls"}"#.into(),
                },
                StreamEvent::Done {
                    stop_reason: "tool_use".into(),
                },
            ],
            vec![
                StreamEvent::ContentDelta {
                    text: "Done".into(),
                },
                StreamEvent::Done {
                    stop_reason: "end_turn".into(),
                },
            ],
        ]);

        let call_log = provider.call_log();
        let _handle = start_daemon_with_mcp(&sock, provider, logs.clone(), mcp_configs).await;
        tokio::time::sleep(WAIT).await;

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (mut reader, mut writer) = stream.into_split();
        send_register(&mut writer).await;

        let req1 = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"tools":[{"name":"bash","description":"Run","parameters":{"type":"object"}}],"messages":[{"role":"user","content":[{"type":"text","text":"Search and list"}]}]}}"#;
        let lines1 = send_and_read_response(&mut writer, &mut reader, req1).await;
        let resp1 = find_final_response(&lines1);
        let result1 = resp1.result.unwrap();
        assert_eq!(result1["stop_reason"], "tool_use");

        let tool_calls = result1["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1, "should only see local tool call");
        assert_eq!(tool_calls[0]["name"], "bash");
        assert_eq!(tool_calls[0]["id"], "tc-local");

        let req2 = r#"{"jsonrpc":"2.0","id":2,"method":"turn","params":{"messages":[{"role":"tool","tool_call_id":"tc-local","content":[{"type":"text","text":"file1.rs\nfile2.rs"}]}]}}"#;
        let lines2 = send_and_read_response(&mut writer, &mut reader, req2).await;
        let resp2 = find_final_response(&lines2);
        let result2 = resp2.result.unwrap();
        assert_eq!(result2["stop_reason"], "end_turn");
        assert_eq!(result2["content"][0]["text"], "Done");

        let log = call_log.lock().unwrap();
        assert_eq!(log.len(), 2, "should have 2 LLM calls");
        let second_msgs = &log[1].messages;

        let has_mcp_result = second_msgs
            .iter()
            .any(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some("tc-mcp"));
        let has_local_result = second_msgs
            .iter()
            .any(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some("tc-local"));
        assert!(
            has_mcp_result,
            "second LLM call should have MCP tool result (tc-mcp)"
        );
        assert!(
            has_local_result,
            "second LLM call should have local tool result (tc-local)"
        );

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn mcp_tools_merged_into_llm_tools_list() {
        let sock = test_socket_path("mcpmerge");
        let logs = test_logs_dir("mcpmerge");

        let tools_list = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"tools": [
                {"name": "mcp_tool", "description": "MCP Tool", "inputSchema": {"type": "object"}}
            ]}
        });
        let (mcp_url, _mcp_handle) = start_mock_mcp_server(vec![tools_list]).await;

        let mcp_configs = vec![ResolvedMcp {
            name: "server".into(),
            url: mcp_url,
            auth_token: "test-token".into(),
            tool_allowlist: None,
        }];

        let provider = MockProvider::new(vec![vec![
            StreamEvent::ContentDelta { text: "OK".into() },
            StreamEvent::Done {
                stop_reason: "end_turn".into(),
            },
        ]]);

        let call_log = provider.call_log();
        let _handle = start_daemon_with_mcp(&sock, provider, logs.clone(), mcp_configs).await;
        tokio::time::sleep(WAIT).await;

        let request = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"tools":[{"name":"bash","description":"Run","parameters":{"type":"object"}}],"messages":[{"role":"user","content":[{"type":"text","text":"Hi"}]}]}}"#;
        let lines = send_and_collect(&sock, request).await;
        let _ = find_final_response(&lines);

        let log = call_log.lock().unwrap();
        let tool_names: Vec<&str> = log[0].tools.iter().map(|t| t.name.as_str()).collect();
        assert!(tool_names.contains(&"bash"), "should have local tool");
        assert!(tool_names.contains(&"mcp_tool"), "should have MCP tool");
        assert_eq!(tool_names.len(), 2);

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn tool_name_collision_local_wins() {
        let sock = test_socket_path("collision");
        let logs = test_logs_dir("collision");

        let tools_list = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"tools": [
                {"name": "bash", "description": "MCP bash (should be excluded)", "inputSchema": {"type": "object"}},
                {"name": "mcp_only", "description": "MCP only tool", "inputSchema": {"type": "object"}}
            ]}
        });
        let (mcp_url, _mcp_handle) = start_mock_mcp_server(vec![tools_list]).await;

        let mcp_configs = vec![ResolvedMcp {
            name: "server".into(),
            url: mcp_url,
            auth_token: "test-token".into(),
            tool_allowlist: None,
        }];

        let provider = MockProvider::new(vec![vec![
            StreamEvent::ContentDelta { text: "OK".into() },
            StreamEvent::Done {
                stop_reason: "end_turn".into(),
            },
        ]]);

        let call_log = provider.call_log();
        let _handle = start_daemon_with_mcp(&sock, provider, logs.clone(), mcp_configs).await;
        tokio::time::sleep(WAIT).await;

        let request = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"tools":[{"name":"bash","description":"Local bash","parameters":{"type":"object"}}],"messages":[{"role":"user","content":[{"type":"text","text":"Hi"}]}]}}"#;
        let lines = send_and_collect(&sock, request).await;
        let _ = find_final_response(&lines);

        let log = call_log.lock().unwrap();
        let tool_names: Vec<&str> = log[0].tools.iter().map(|t| t.name.as_str()).collect();

        assert_eq!(
            tool_names.iter().filter(|&&n| n == "bash").count(),
            1,
            "bash should appear exactly once (local wins)"
        );
        assert!(
            tool_names.contains(&"mcp_only"),
            "non-colliding MCP tool should be present"
        );
        assert_eq!(tool_names.len(), 2, "should have 2 tools total");

        let bash_tool = log[0].tools.iter().find(|t| t.name == "bash").unwrap();
        assert_eq!(
            bash_tool.description, "Local bash",
            "should be the local tool, not the MCP one"
        );

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn mcp_init_failure_returns_error() {
        let sock = test_socket_path("mcpfail");
        let logs = test_logs_dir("mcpfail");

        let mcp_configs = vec![ResolvedMcp {
            name: "dead-server".into(),
            url: "http://127.0.0.1:1".into(),
            auth_token: "test-token".into(),
            tool_allowlist: None,
        }];

        let provider = MockProvider::new(vec![]);
        let _handle = start_daemon_with_mcp(&sock, provider, logs.clone(), mcp_configs).await;
        tokio::time::sleep(WAIT).await;

        let request = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"messages":[{"role":"user","content":[{"type":"text","text":"Hi"}]}]}}"#;
        let lines = send_and_collect(&sock, request).await;

        assert!(!lines.is_empty(), "should get a response");
        let resp: AnyResponse = serde_json::from_str(&lines[lines.len() - 1]).unwrap();
        assert!(
            resp.error.is_some(),
            "should be an error response when MCP init fails"
        );
        let err_msg = resp.error.unwrap()["message"].as_str().unwrap().to_string();
        assert!(
            err_msg.contains("MCP init"),
            "error should mention MCP init, got: {err_msg}"
        );

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }
}

// --- Smoke test: full startup path ---

#[tokio::test]
async fn smoke_full_startup_path() {
    let base = tempfile::tempdir().unwrap();
    let agents_dir = base.path().join("agents");
    let agent_dir = agents_dir.join("smoke-agent");
    let sockets_dir = base.path().join("sockets");
    let logs_dir = base.path().join("logs");
    let secrets_dir = base.path().join("secrets");

    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&sockets_dir).unwrap();
    std::fs::create_dir_all(&logs_dir).unwrap();
    std::fs::create_dir_all(&secrets_dir).unwrap();

    let key_path = secrets_dir.join("api_key");
    std::fs::write(&key_path, "smoke-test-key").unwrap();

    std::fs::write(
        agent_dir.join("tightbeam.toml"),
        format!(
            "[llm]\nprovider = \"anthropic\"\nmodel = \"smoke-model\"\napi_key_file = \"{}\"",
            key_path.display()
        ),
    )
    .unwrap();

    // Step 1: Directory scanning + secret file reading
    let configs = tightbeam_daemon::registration::load_agents(&agents_dir).unwrap();
    assert_eq!(configs.len(), 1);
    assert!(configs.contains_key("smoke-agent"));
    assert_eq!(configs["smoke-agent"].llm.api_key, "smoke-test-key");
    assert_eq!(configs["smoke-agent"].llm.model, "smoke-model");

    // Step 2: Bind socket, start daemon with MockProvider
    let sock_path = sockets_dir.join("smoke-agent.sock");
    let listener = bind_agent_socket(&sock_path).unwrap();

    let provider = MockProvider::new(vec![vec![
        StreamEvent::ContentDelta {
            text: "Hello from smoke test".into(),
        },
        StreamEvent::Done {
            stop_reason: "end_turn".into(),
        },
    ]]);

    let mut profiles = HashMap::new();
    profiles.insert(
        "smoke-agent".to_string(),
        configs.into_values().next().unwrap(),
    );
    let profiles: ProfileMap = Arc::new(RwLock::new(profiles));

    let mut providers_map: HashMap<tightbeam_providers::Provider, Box<dyn LlmProvider>> =
        HashMap::new();
    providers_map.insert(tightbeam_providers::Provider::Anthropic, Box::new(provider));
    let providers: ProviderMap = Arc::new(RwLock::new(providers_map));

    let conversations: ConversationMap = Arc::new(RwLock::new(HashMap::new()));
    let mcp_managers: McpManagerMap = Arc::new(RwLock::new(HashMap::new()));

    let listeners = vec![("smoke-agent".to_string(), listener)];

    let logs = logs_dir.clone();
    let sockets = sockets_dir.clone();
    let agents = agents_dir.clone();
    tokio::spawn(async move {
        run_daemon(
            listeners,
            profiles,
            conversations,
            providers,
            mcp_managers,
            logs,
            sockets,
            agents,
        )
        .await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Step 3: Connect, register, send turn, verify response
    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (mut reader, mut writer) = stream.into_split();

    send_register(&mut writer).await;

    let turn = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"messages":[{"role":"user","content":[{"type":"text","text":"Smoke test"}]}]}}"#;
    let responses = send_and_read_response(&mut writer, &mut reader, turn).await;

    let final_resp: serde_json::Value = serde_json::from_str(responses.last().unwrap()).unwrap();
    assert_eq!(final_resp["result"]["stop_reason"], "end_turn");

    let has_content = responses
        .iter()
        .any(|r| r.contains("Hello from smoke test"));
    assert!(
        has_content,
        "should contain smoke test output: {responses:?}"
    );
}

// --- Send integration tests ---

mod send_integration {
    use super::*;

    async fn send_rpc_frame(writer: &mut tokio::net::unix::OwnedWriteHalf, id: u64, text: &str) {
        let rpc = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "send",
            "params": {"content": [{"type": "text", "text": text}]}
        });
        let payload = serde_json::to_vec(&rpc).unwrap();
        write_frame(writer, &payload).await.unwrap();
    }

    #[tokio::test]
    async fn send_without_runtime_returns_error() {
        let sock = test_socket_path("send-nort");
        let logs = test_logs_dir("send-nort");

        let provider = MockProvider::new(vec![]);
        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (mut reader, mut writer) = stream.into_split();
        send_rpc_frame(&mut writer, 1, "Hello").await;

        let resp = read_one_frame(&mut reader).await;
        assert!(resp.get("error").is_some(), "should get error: {resp}");
        let msg = resp["error"]["message"].as_str().unwrap();
        assert!(
            msg.contains("not connected"),
            "error should mention runtime not connected, got: {msg}"
        );

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn send_delivers_to_runtime_and_streams_output() {
        let sock = test_socket_path("send-deliver");
        let logs = test_logs_dir("send-deliver");

        let provider = MockProvider::new(vec![vec![
            StreamEvent::ContentDelta {
                text: "Hello ".into(),
            },
            StreamEvent::ContentDelta {
                text: "back".into(),
            },
            StreamEvent::Done {
                stop_reason: "end_turn".into(),
            },
        ]]);

        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        // Connect as runtime
        let rt_stream = UnixStream::connect(&sock).await.unwrap();
        let (mut rt_reader, mut rt_writer) = rt_stream.into_split();
        send_register(&mut rt_writer).await;
        tokio::time::sleep(WAIT).await;

        // Connect as subscriber and send
        let sub_stream = UnixStream::connect(&sock).await.unwrap();
        let (mut sub_reader, mut sub_writer) = sub_stream.into_split();
        send_rpc_frame(&mut sub_writer, 1, "Hi agent").await;

        // Subscriber should get "delivered" response
        let resp = read_one_frame(&mut sub_reader).await;
        assert_eq!(resp["result"]["status"], "delivered");

        // Runtime should receive human_message
        let human = read_one_frame(&mut rt_reader).await;
        assert_eq!(human["method"], "human_message");
        assert_eq!(human["params"]["content"][0]["text"], "Hi agent");

        // Runtime sends turn back
        let turn_req = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"messages":[{"role":"user","content":[{"type":"text","text":"Hi agent"}]}]}}"#;
        write_frame(&mut rt_writer, turn_req.as_bytes())
            .await
            .unwrap();

        // Read runtime's response (notifications + final)
        loop {
            let frame = read_one_frame(&mut rt_reader).await;
            if frame.get("id").is_some() {
                break;
            }
        }

        // Subscriber should receive text output notifications and end_turn
        let mut sub_frames = Vec::new();
        loop {
            let frame = read_one_frame(&mut sub_reader).await;
            let method = frame["method"].as_str().unwrap_or("").to_string();
            sub_frames.push(frame);
            if method == "end_turn" {
                break;
            }
        }

        let text_notifs: Vec<_> = sub_frames
            .iter()
            .filter(|f| f["method"] == "output")
            .collect();
        assert!(
            !text_notifs.is_empty(),
            "subscriber should receive text output notifications"
        );

        let last = sub_frames.last().unwrap();
        assert_eq!(last["method"], "end_turn");

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn send_when_busy_queues() {
        let sock = test_socket_path("send-queue");
        let logs = test_logs_dir("send-queue");

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

        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        // Connect runtime
        let rt_stream = UnixStream::connect(&sock).await.unwrap();
        let (mut rt_reader, mut rt_writer) = rt_stream.into_split();
        send_register(&mut rt_writer).await;
        tokio::time::sleep(WAIT).await;

        // Subscriber 1 sends (should be delivered)
        let sub1_stream = UnixStream::connect(&sock).await.unwrap();
        let (mut sub1_reader, mut sub1_writer) = sub1_stream.into_split();
        send_rpc_frame(&mut sub1_writer, 1, "First message").await;

        let resp1 = read_one_frame(&mut sub1_reader).await;
        assert_eq!(resp1["result"]["status"], "delivered");

        // Subscriber 2 sends while busy (should be queued)
        let sub2_stream = UnixStream::connect(&sock).await.unwrap();
        let (mut sub2_reader, mut sub2_writer) = sub2_stream.into_split();
        send_rpc_frame(&mut sub2_writer, 1, "Second message").await;

        let resp2 = read_one_frame(&mut sub2_reader).await;
        assert_eq!(resp2["result"]["status"], "queued");

        // Runtime processes first message
        let human1 = read_one_frame(&mut rt_reader).await;
        assert_eq!(human1["method"], "human_message");

        let turn1 = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"messages":[{"role":"user","content":[{"type":"text","text":"First message"}]}]}}"#;
        write_frame(&mut rt_writer, turn1.as_bytes()).await.unwrap();

        // Read runtime response for turn 1
        loop {
            let frame = read_one_frame(&mut rt_reader).await;
            if frame.get("id").is_some() {
                break;
            }
        }

        // Sub1 (active subscriber) should receive end_turn for turn 1
        loop {
            let frame = read_one_frame(&mut sub1_reader).await;
            if frame["method"] == "end_turn" {
                break;
            }
        }

        // After turn 1 completes, queued message should be delivered
        let human2 = read_one_frame(&mut rt_reader).await;
        assert_eq!(human2["method"], "human_message");
        assert_eq!(human2["params"]["content"][0]["text"], "Second message");

        // Sub2 should get "delivered" notification
        let delivered = read_one_frame(&mut sub2_reader).await;
        assert_eq!(delivered["method"], "delivered");

        // Runtime processes second message
        let turn2 = r#"{"jsonrpc":"2.0","id":2,"method":"turn","params":{"messages":[{"role":"user","content":[{"type":"text","text":"Second message"}]}]}}"#;
        write_frame(&mut rt_writer, turn2.as_bytes()).await.unwrap();

        // Read runtime response for turn 2
        loop {
            let frame = read_one_frame(&mut rt_reader).await;
            if frame.get("id").is_some() {
                break;
            }
        }

        // Sub2 should get end_turn
        loop {
            let frame = read_one_frame(&mut sub2_reader).await;
            if frame["method"] == "end_turn" {
                break;
            }
        }

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn runtime_disconnect_notifies_subscribers() {
        let sock = test_socket_path("send-rtdc");
        let logs = test_logs_dir("send-rtdc");

        let provider = MockProvider::new(vec![]);
        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        // Connect runtime
        let rt_stream = UnixStream::connect(&sock).await.unwrap();
        let (rt_reader, mut rt_writer) = rt_stream.into_split();
        send_register(&mut rt_writer).await;
        tokio::time::sleep(WAIT).await;

        // Connect subscriber and send
        let sub_stream = UnixStream::connect(&sock).await.unwrap();
        let (mut sub_reader, mut sub_writer) = sub_stream.into_split();
        send_rpc_frame(&mut sub_writer, 1, "Hello").await;

        let resp = read_one_frame(&mut sub_reader).await;
        assert_eq!(resp["result"]["status"], "delivered");

        // Disconnect runtime
        drop(rt_reader);
        drop(rt_writer);

        // Subscriber should receive disconnect error
        tokio::time::sleep(WAIT).await;
        let frame = read_one_frame(&mut sub_reader).await;
        assert_eq!(frame["method"], "error");
        assert!(frame["params"]["message"]
            .as_str()
            .unwrap()
            .contains("disconnected"));

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn tool_use_events_not_broadcast_to_subscribers() {
        let sock = test_socket_path("send-toolfilter");
        let logs = test_logs_dir("send-toolfilter");

        let provider = MockProvider::new(vec![
            vec![
                StreamEvent::ContentDelta {
                    text: "Let me run that".into(),
                },
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
            vec![
                StreamEvent::ContentDelta {
                    text: "Here are the files".into(),
                },
                StreamEvent::Done {
                    stop_reason: "end_turn".into(),
                },
            ],
        ]);

        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        let rt_stream = UnixStream::connect(&sock).await.unwrap();
        let (mut rt_reader, mut rt_writer) = rt_stream.into_split();
        send_register(&mut rt_writer).await;
        tokio::time::sleep(WAIT).await;

        let sub_stream = UnixStream::connect(&sock).await.unwrap();
        let (mut sub_reader, mut sub_writer) = sub_stream.into_split();
        send_rpc_frame(&mut sub_writer, 1, "List files").await;

        let resp = read_one_frame(&mut sub_reader).await;
        assert_eq!(resp["result"]["status"], "delivered");

        let _human = read_one_frame(&mut rt_reader).await;
        let turn1 = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"tools":[{"name":"bash","description":"Run","parameters":{"type":"object"}}],"messages":[{"role":"user","content":[{"type":"text","text":"List files"}]}]}}"#;
        write_frame(&mut rt_writer, turn1.as_bytes()).await.unwrap();

        loop {
            let frame = read_one_frame(&mut rt_reader).await;
            if frame.get("id").is_some() {
                assert_eq!(frame["result"]["stop_reason"], "tool_use");
                break;
            }
        }

        let turn2 = r#"{"jsonrpc":"2.0","id":2,"method":"turn","params":{"messages":[{"role":"tool","tool_call_id":"tc-1","content":[{"type":"text","text":"main.rs\nlib.rs"}]}]}}"#;
        write_frame(&mut rt_writer, turn2.as_bytes()).await.unwrap();

        loop {
            let frame = read_one_frame(&mut rt_reader).await;
            if frame.get("id").is_some() {
                assert_eq!(frame["result"]["stop_reason"], "end_turn");
                break;
            }
        }

        let mut sub_frames = Vec::new();
        loop {
            let frame = read_one_frame(&mut sub_reader).await;
            let method = frame["method"].as_str().unwrap_or("").to_string();
            sub_frames.push(frame);
            if method == "end_turn" {
                break;
            }
        }

        let text_notifs: Vec<_> = sub_frames
            .iter()
            .filter(|f| {
                f["method"] == "output" && f["params"]["data"]["type"].as_str() == Some("text")
            })
            .collect();
        assert!(
            !text_notifs.is_empty(),
            "subscriber should receive text notifications"
        );

        let tool_notifs: Vec<_> = sub_frames
            .iter()
            .filter(|f| {
                if let Some(dtype) = f["params"]["data"]["type"].as_str() {
                    dtype == "tool_use_start" || dtype == "tool_use_input"
                } else {
                    false
                }
            })
            .collect();
        assert!(
            tool_notifs.is_empty(),
            "subscriber should NOT receive tool_use events, got: {tool_notifs:?}"
        );

        let all_text: String = text_notifs
            .iter()
            .filter_map(|f| f["params"]["data"]["text"].as_str())
            .collect();
        assert!(
            all_text.contains("Let me run that\n"),
            "subscriber output should have newline after first turn text, got: {all_text:?}"
        );

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn tool_use_does_not_trigger_premature_end_turn() {
        let sock = test_socket_path("send-toolnofin");
        let logs = test_logs_dir("send-toolnofin");

        let provider = MockProvider::new(vec![
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
            vec![
                StreamEvent::ContentDelta {
                    text: "Done".into(),
                },
                StreamEvent::Done {
                    stop_reason: "end_turn".into(),
                },
            ],
        ]);

        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        let rt_stream = UnixStream::connect(&sock).await.unwrap();
        let (mut rt_reader, mut rt_writer) = rt_stream.into_split();
        send_register(&mut rt_writer).await;
        tokio::time::sleep(WAIT).await;

        let sub_stream = UnixStream::connect(&sock).await.unwrap();
        let (mut sub_reader, mut sub_writer) = sub_stream.into_split();
        send_rpc_frame(&mut sub_writer, 1, "Run command").await;

        let resp = read_one_frame(&mut sub_reader).await;
        assert_eq!(resp["result"]["status"], "delivered");

        let _human = read_one_frame(&mut rt_reader).await;
        let turn1 = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"tools":[{"name":"bash","description":"Run","parameters":{"type":"object"}}],"messages":[{"role":"user","content":[{"type":"text","text":"Run command"}]}]}}"#;
        write_frame(&mut rt_writer, turn1.as_bytes()).await.unwrap();

        loop {
            let frame = read_one_frame(&mut rt_reader).await;
            if frame.get("id").is_some() {
                break;
            }
        }

        // After ToolUse, subscriber should NOT receive end_turn yet
        let no_end_turn = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            read_one_frame(&mut sub_reader),
        )
        .await;
        assert!(
            no_end_turn.is_err(),
            "subscriber should NOT receive end_turn after ToolUse (should timeout)"
        );

        let turn2 = r#"{"jsonrpc":"2.0","id":2,"method":"turn","params":{"messages":[{"role":"tool","tool_call_id":"tc-1","content":[{"type":"text","text":"file1.rs"}]}]}}"#;
        write_frame(&mut rt_writer, turn2.as_bytes()).await.unwrap();

        loop {
            let frame = read_one_frame(&mut rt_reader).await;
            if frame.get("id").is_some() {
                break;
            }
        }

        // NOW subscriber should get end_turn
        loop {
            let frame = read_one_frame(&mut sub_reader).await;
            if frame["method"] == "end_turn" {
                break;
            }
        }

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn max_tokens_triggers_finish_turn() {
        let sock = test_socket_path("send-maxtoken");
        let logs = test_logs_dir("send-maxtoken");

        let provider = MockProvider::new(vec![vec![
            StreamEvent::ContentDelta {
                text: "partial...".into(),
            },
            StreamEvent::Done {
                stop_reason: "max_tokens".into(),
            },
        ]]);

        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        let rt_stream = UnixStream::connect(&sock).await.unwrap();
        let (mut rt_reader, mut rt_writer) = rt_stream.into_split();
        send_register(&mut rt_writer).await;
        tokio::time::sleep(WAIT).await;

        let sub_stream = UnixStream::connect(&sock).await.unwrap();
        let (mut sub_reader, mut sub_writer) = sub_stream.into_split();
        send_rpc_frame(&mut sub_writer, 1, "Generate long text").await;

        let resp = read_one_frame(&mut sub_reader).await;
        assert_eq!(resp["result"]["status"], "delivered");

        let _human = read_one_frame(&mut rt_reader).await;
        let turn = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"messages":[{"role":"user","content":[{"type":"text","text":"Generate long text"}]}]}}"#;
        write_frame(&mut rt_writer, turn.as_bytes()).await.unwrap();

        loop {
            let frame = read_one_frame(&mut rt_reader).await;
            if frame.get("id").is_some() {
                assert_eq!(frame["result"]["stop_reason"], "max_tokens");
                break;
            }
        }

        // max_tokens should trigger finish_turn → subscriber gets end_turn
        loop {
            let frame = read_one_frame(&mut sub_reader).await;
            if frame["method"] == "end_turn" {
                break;
            }
        }

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn send_with_image_delivers_image_block() {
        let sock = test_socket_path("send-img");
        let logs = test_logs_dir("send-img");

        let provider = MockProvider::new(vec![vec![
            StreamEvent::ContentDelta {
                text: "I see an image".into(),
            },
            StreamEvent::Done {
                stop_reason: "end_turn".into(),
            },
        ]]);

        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        let rt_stream = UnixStream::connect(&sock).await.unwrap();
        let (mut rt_reader, mut rt_writer) = rt_stream.into_split();
        send_register(&mut rt_writer).await;
        tokio::time::sleep(WAIT).await;

        let sub_stream = UnixStream::connect(&sock).await.unwrap();
        let (mut sub_reader, mut sub_writer) = sub_stream.into_split();

        // Send RPC with text + file_incoming
        let rpc = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "send",
            "params": {
                "content": [
                    {"type": "text", "text": "Describe this"},
                    {"type": "file_incoming", "filename": "test.png", "mime_type": "image/png", "size": 4}
                ]
            }
        });
        write_frame(&mut sub_writer, &serde_json::to_vec(&rpc).unwrap())
            .await
            .unwrap();

        // Read response BEFORE sending file frame
        let resp = read_one_frame(&mut sub_reader).await;
        assert_eq!(resp["result"]["status"], "delivered");

        // Send file frame (4 bytes of fake PNG data)
        tightbeam_protocol::framing::write_frame_header(&mut sub_writer, 4)
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut sub_writer, b"\x89PNG")
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::flush(&mut sub_writer)
            .await
            .unwrap();

        // Runtime receives human_message with Image block (not FileIncoming)
        let human = read_one_frame(&mut rt_reader).await;
        assert_eq!(human["method"], "human_message");
        let content = human["params"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["media_type"], "image/png");

        // Verify base64 round-trip
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(content[1]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, b"\x89PNG");

        // Complete the turn
        let turn = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"messages":[{"role":"user","content":[{"type":"text","text":"Describe this"}]}]}}"#;
        write_frame(&mut rt_writer, turn.as_bytes()).await.unwrap();

        loop {
            let frame = read_one_frame(&mut rt_reader).await;
            if frame.get("id").is_some() {
                break;
            }
        }
        loop {
            let frame = read_one_frame(&mut sub_reader).await;
            if frame["method"] == "end_turn" {
                break;
            }
        }

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn send_rejects_unsupported_file_type() {
        let sock = test_socket_path("send-badmime");
        let logs = test_logs_dir("send-badmime");

        let provider = MockProvider::new(vec![]);
        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        let rt_stream = UnixStream::connect(&sock).await.unwrap();
        let (_rt_reader, mut rt_writer) = rt_stream.into_split();
        send_register(&mut rt_writer).await;
        tokio::time::sleep(WAIT).await;

        let sub_stream = UnixStream::connect(&sock).await.unwrap();
        let (mut sub_reader, mut sub_writer) = sub_stream.into_split();

        let rpc = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "send",
            "params": {
                "content": [
                    {"type": "file_incoming", "filename": "data.csv", "mime_type": "text/csv", "size": 100}
                ]
            }
        });
        write_frame(&mut sub_writer, &serde_json::to_vec(&rpc).unwrap())
            .await
            .unwrap();

        let resp = read_one_frame(&mut sub_reader).await;
        assert!(resp.get("error").is_some(), "should reject: {resp}");
        let msg = resp["error"]["message"].as_str().unwrap();
        assert!(msg.contains("unsupported file type"), "got: {msg}");

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn send_with_multiple_images_preserves_order() {
        let sock = test_socket_path("send-multiimg");
        let logs = test_logs_dir("send-multiimg");

        let provider = MockProvider::new(vec![vec![
            StreamEvent::ContentDelta {
                text: "Two images".into(),
            },
            StreamEvent::Done {
                stop_reason: "end_turn".into(),
            },
        ]]);

        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        let rt_stream = UnixStream::connect(&sock).await.unwrap();
        let (mut rt_reader, mut rt_writer) = rt_stream.into_split();
        send_register(&mut rt_writer).await;
        tokio::time::sleep(WAIT).await;

        let sub_stream = UnixStream::connect(&sock).await.unwrap();
        let (mut sub_reader, mut sub_writer) = sub_stream.into_split();

        let rpc = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "send",
            "params": {
                "content": [
                    {"type": "text", "text": "Compare these"},
                    {"type": "file_incoming", "filename": "a.png", "mime_type": "image/png", "size": 3},
                    {"type": "file_incoming", "filename": "b.jpg", "mime_type": "image/jpeg", "size": 5}
                ]
            }
        });
        write_frame(&mut sub_writer, &serde_json::to_vec(&rpc).unwrap())
            .await
            .unwrap();

        let resp = read_one_frame(&mut sub_reader).await;
        assert_eq!(resp["result"]["status"], "delivered");

        // Send two file frames in order
        tightbeam_protocol::framing::write_frame_header(&mut sub_writer, 3)
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut sub_writer, b"AAA")
            .await
            .unwrap();
        tightbeam_protocol::framing::write_frame_header(&mut sub_writer, 5)
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut sub_writer, b"BBBBB")
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::flush(&mut sub_writer)
            .await
            .unwrap();

        // Runtime receives human_message with correct ordering
        let human = read_one_frame(&mut rt_reader).await;
        assert_eq!(human["method"], "human_message");
        let content = human["params"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "Compare these");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["media_type"], "image/png");
        assert_eq!(content[2]["type"], "image");
        assert_eq!(content[2]["media_type"], "image/jpeg");

        use base64::Engine;
        let decoded1 = base64::engine::general_purpose::STANDARD
            .decode(content[1]["data"].as_str().unwrap())
            .unwrap();
        let decoded2 = base64::engine::general_purpose::STANDARD
            .decode(content[2]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded1, b"AAA", "first image should be AAA");
        assert_eq!(decoded2, b"BBBBB", "second image should be BBBBB");

        // Complete turn
        let turn = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"messages":[{"role":"user","content":[{"type":"text","text":"Compare these"}]}]}}"#;
        write_frame(&mut rt_writer, turn.as_bytes()).await.unwrap();
        loop {
            let frame = read_one_frame(&mut rt_reader).await;
            if frame.get("id").is_some() {
                break;
            }
        }
        loop {
            let frame = read_one_frame(&mut sub_reader).await;
            if frame["method"] == "end_turn" {
                break;
            }
        }

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn text_only_send_unaffected_by_file_support() {
        let sock = test_socket_path("send-textonly2");
        let logs = test_logs_dir("send-textonly2");

        let provider = MockProvider::new(vec![vec![
            StreamEvent::ContentDelta {
                text: "Hello".into(),
            },
            StreamEvent::Done {
                stop_reason: "end_turn".into(),
            },
        ]]);

        let _handle = start_daemon(&sock, provider, logs.clone()).await;
        tokio::time::sleep(WAIT).await;

        let rt_stream = UnixStream::connect(&sock).await.unwrap();
        let (mut rt_reader, mut rt_writer) = rt_stream.into_split();
        send_register(&mut rt_writer).await;
        tokio::time::sleep(WAIT).await;

        let sub_stream = UnixStream::connect(&sock).await.unwrap();
        let (mut sub_reader, mut sub_writer) = sub_stream.into_split();
        send_rpc_frame(&mut sub_writer, 1, "Just text").await;

        let resp = read_one_frame(&mut sub_reader).await;
        assert_eq!(resp["result"]["status"], "delivered");

        let human = read_one_frame(&mut rt_reader).await;
        assert_eq!(human["params"]["content"][0]["type"], "text");

        let turn = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"messages":[{"role":"user","content":[{"type":"text","text":"Just text"}]}]}}"#;
        write_frame(&mut rt_writer, turn.as_bytes()).await.unwrap();

        loop {
            let frame = read_one_frame(&mut rt_reader).await;
            if frame.get("id").is_some() {
                break;
            }
        }
        loop {
            let frame = read_one_frame(&mut sub_reader).await;
            if frame["method"] == "end_turn" {
                break;
            }
        }

        let _ = std::fs::remove_dir_all(&logs);
        let _ = std::fs::remove_file(&sock);
    }
}

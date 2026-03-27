use std::collections::HashMap;
use std::path::Path;

use crate::config::RuntimeConfig;
use crate::connection::DaemonConnection;
use crate::tools;
use tightbeam_protocol::{content_text, ContentBlock, Message, StopReason, TurnRequest, TurnResponse};

pub(crate) async fn run_agent(config: RuntimeConfig, agents_dir: &Path) -> Result<(), String> {
    let agents = discover_agents(agents_dir).await?;
    let tool_defs = tools::tool_definitions(&config.tools);
    let conn = DaemonConnection::connect(&config.socket_path).await?;

    if agents.len() == 1 {
        run_single_agent(config, agents, tool_defs, conn).await
    } else {
        run_multi_agent(config, agents, tool_defs, conn).await
    }
}

async fn discover_agents(agents_dir: &Path) -> Result<HashMap<String, String>, String> {
    let mut agents = HashMap::new();
    let entries = std::fs::read_dir(agents_dir)
        .map_err(|e| format!("failed to read agents directory {}: {e}", agents_dir.display()))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("failed to read directory entry: {e}"))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("invalid agent directory name: {}", path.display()))?
            .to_string();
        let prompt = crate::prompt::load_system_prompt(&path).await?;
        agents.insert(name, prompt);
    }

    if agents.is_empty() {
        return Err(format!(
            "no agent directories found in {}",
            agents_dir.display()
        ));
    }

    Ok(agents)
}

async fn run_single_agent(
    config: RuntimeConfig,
    agents: HashMap<String, String>,
    tool_defs: Vec<tightbeam_protocol::ToolDefinition>,
    mut conn: DaemonConnection,
) -> Result<(), String> {
    let (name, system_prompt) = agents.into_iter().next().unwrap();
    let mut tool_defs = Some(tool_defs);

    loop {
        let human = conn.wait_for_human_message().await?;

        let user_msg = Message {
            role: "user".into(),
            content: Some(human.content),
            tool_calls: None,
            tool_call_id: None,
            is_error: None,
            agent: None,
        };

        let request = TurnRequest {
            system: Some(system_prompt.clone()),
            tools: tool_defs.take(),
            messages: vec![user_msg],
            agent: Some(name.clone()),
        };

        let id = conn.send_turn(&request).await?;
        tool_loop(&config, &mut conn, id, &name).await?;
    }
}

async fn run_multi_agent(
    config: RuntimeConfig,
    agents: HashMap<String, String>,
    tool_defs: Vec<tightbeam_protocol::ToolDefinition>,
    mut conn: DaemonConnection,
) -> Result<(), String> {
    let router_prompt = agents
        .get("router")
        .ok_or("multi-agent mode requires a 'router' agent directory")?
        .clone();

    let mut active_agent = agents
        .keys()
        .find(|k| *k != "router")
        .ok_or("no non-router agent directories found")?
        .clone();

    let mut tool_defs = Some(tool_defs);

    loop {
        let human = conn.wait_for_human_message().await?;

        let user_msg = Message {
            role: "user".into(),
            content: Some(human.content),
            tool_calls: None,
            tool_call_id: None,
            is_error: None,
            agent: None,
        };

        // Router turn
        let router_req = TurnRequest {
            system: Some(router_prompt.clone()),
            tools: None,
            messages: vec![user_msg],
            agent: Some("router".into()),
        };
        let router_id = conn.send_turn(&router_req).await?;
        let router_resp = conn.read_turn_response(router_id).await?;
        active_agent = parse_router_response(&router_resp, &agents, &active_agent);

        // Agent turn (user message already in daemon history from router turn)
        let agent_req = TurnRequest {
            system: Some(agents[&active_agent].clone()),
            tools: tool_defs.take(),
            messages: vec![],
            agent: Some(active_agent.clone()),
        };
        let agent_id = conn.send_turn(&agent_req).await?;
        tool_loop(&config, &mut conn, agent_id, &active_agent).await?;
    }
}

fn parse_router_response(
    response: &TurnResponse,
    agents: &HashMap<String, String>,
    current: &str,
) -> String {
    let chosen = content_text(&response.content)
        .unwrap_or("")
        .trim()
        .to_lowercase();

    if chosen.is_empty() || chosen == "router" || !agents.contains_key(&chosen) {
        if !chosen.is_empty() {
            eprintln!("tightbeam-runtime: router returned unknown agent '{chosen}', keeping '{current}'");
        }
        return current.to_string();
    }

    chosen
}

async fn tool_loop(
    config: &RuntimeConfig,
    conn: &mut DaemonConnection,
    initial_id: u64,
    agent: &str,
) -> Result<(), String> {
    let mut id = initial_id;
    let mut iterations = 0u32;

    loop {
        let response = conn.read_turn_response(id).await?;

        match response.stop_reason {
            StopReason::EndTurn => return Ok(()),
            StopReason::MaxTokens => {
                eprintln!("tightbeam-runtime: max_tokens reached, ending turn");
                return Ok(());
            }
            StopReason::ToolUse => {
                iterations += 1;
                if iterations >= config.max_iterations {
                    eprintln!(
                        "tightbeam-runtime: iteration limit ({}) reached, stopping",
                        config.max_iterations
                    );
                    return Ok(());
                }

                let tool_calls = match response.tool_calls {
                    Some(tc) if !tc.is_empty() => tc,
                    _ => return Ok(()),
                };

                let mut handles = Vec::with_capacity(tool_calls.len());
                for tc in &tool_calls {
                    let name = tc.name.clone();
                    let input = tc.input.clone();
                    let max_chars = config.max_output_chars;
                    handles.push(tokio::spawn(async move {
                        tools::execute_tool(&name, &input, max_chars).await
                    }));
                }

                let mut tool_result_messages = Vec::with_capacity(tool_calls.len());
                for (i, handle) in handles.into_iter().enumerate() {
                    let (output, is_error) = handle.join_err(i).await?;
                    tool_result_messages.push(Message {
                        role: "tool".into(),
                        content: Some(ContentBlock::text_content(output)),
                        tool_calls: None,
                        tool_call_id: Some(tool_calls[i].id.clone()),
                        is_error: if is_error { Some(true) } else { None },
                        agent: None,
                    });
                }

                let turn = TurnRequest {
                    system: None,
                    tools: None,
                    messages: tool_result_messages,
                    agent: Some(agent.to_string()),
                };
                id = conn.send_turn(&turn).await?;
            }
        }
    }
}

trait JoinHandleExt<T> {
    async fn join_err(self, index: usize) -> Result<T, String>;
}

impl<T> JoinHandleExt<T> for tokio::task::JoinHandle<T> {
    async fn join_err(self, index: usize) -> Result<T, String> {
        self.await
            .map_err(|e| format!("tool task {index} panicked: {e}"))
    }
}

#[cfg(test)]
mod agent_tests {
    use super::*;
    use tightbeam_protocol::framing::{read_frame, write_frame};
    use tightbeam_protocol::{ContentBlock, ToolCall, TurnResponse};

    struct MockDaemon {
        reader: tokio::net::unix::OwnedReadHalf,
        writer: tokio::net::unix::OwnedWriteHalf,
    }

    impl MockDaemon {
        async fn read_turn(&mut self) -> serde_json::Value {
            let frame = read_frame(&mut self.reader).await.unwrap().unwrap();
            serde_json::from_slice(&frame).unwrap()
        }

        async fn write_json(&mut self, value: &serde_json::Value) {
            let payload = serde_json::to_vec(value).unwrap();
            write_frame(&mut self.writer, &payload).await.unwrap();
        }

        async fn send_human_message(&mut self, content: &str) {
            self.write_json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "human_message",
                "params": {"content": [{"type": "text", "text": content}]}
            }))
            .await;
        }

        async fn send_response(&mut self, id: u64, response: &TurnResponse) {
            self.write_json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": response
            }))
            .await;
        }
    }

    async fn setup_agent(
        name: &str,
        max_iterations: u32,
    ) -> (
        tokio::task::JoinHandle<Result<(), String>>,
        MockDaemon,
        tempfile::TempDir,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        let agent_dir = agents_dir.join("test");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("prompt.md"), "You are a test agent.").unwrap();

        let sock_dir =
            std::env::temp_dir().join(format!("tb-agent-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&sock_dir);
        std::fs::create_dir_all(&sock_dir).unwrap();
        let sock_path = sock_dir.join("test.sock");

        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();

        let config = RuntimeConfig {
            tools: vec!["bash".into()],
            socket_path: sock_path,
            max_iterations,
            max_output_chars: 30000,
        };

        let agent_handle = tokio::spawn(async move { run_agent(config, &agents_dir).await });

        let (stream, _) = listener.accept().await.unwrap();
        let (mut read, write) = stream.into_split();

        // Consume the register frame sent by DaemonConnection::connect()
        let frame = read_frame(&mut read).await.unwrap().unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(parsed["method"], "register");

        let mock = MockDaemon {
            reader: read,
            writer: write,
        };

        (agent_handle, mock, tmp)
    }

    #[tokio::test]
    async fn agent_sends_system_every_turn_and_tools_on_first() {
        let (agent_handle, mut mock, _tmp) = setup_agent("first", 100).await;

        mock.send_human_message("Hello").await;
        let turn = mock.read_turn().await;

        assert_eq!(turn["method"], "turn");
        assert!(
            turn["params"]["system"].is_string(),
            "first turn must include system prompt"
        );
        assert_eq!(turn["params"]["system"], "You are a test agent.");
        assert!(
            turn["params"]["tools"].is_array(),
            "first turn must include tools"
        );
        assert!(!turn["params"]["tools"].as_array().unwrap().is_empty());
        assert_eq!(turn["params"]["messages"][0]["role"], "user");
        assert_eq!(turn["params"]["messages"][0]["content"][0]["text"], "Hello");

        mock.send_response(
            turn["id"].as_u64().unwrap(),
            &TurnResponse {
                stop_reason: StopReason::EndTurn,
                content: Some(ContentBlock::text_content("Hi there!")),
                tool_calls: None,
            },
        )
        .await;

        // Send second human message — system included, tools omitted
        mock.send_human_message("Second").await;
        let turn2 = mock.read_turn().await;

        assert_eq!(
            turn2["params"]["system"], "You are a test agent.",
            "system prompt must be sent on every turn"
        );
        assert!(turn2["params"].get("tools").is_none() || turn2["params"]["tools"].is_null());
        assert_eq!(
            turn2["params"]["messages"][0]["content"][0]["text"],
            "Second"
        );

        drop(mock);
        let _ = agent_handle.await;
    }

    #[tokio::test]
    async fn agent_executes_tools_and_sends_results() {
        let (agent_handle, mut mock, _tmp) = setup_agent("toolexec", 100).await;

        mock.send_human_message("List files").await;
        let turn1 = mock.read_turn().await;
        let id1 = turn1["id"].as_u64().unwrap();

        mock.send_response(
            id1,
            &TurnResponse {
                stop_reason: StopReason::ToolUse,
                content: None,
                tool_calls: Some(vec![ToolCall {
                    id: "tc-1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "echo tool_output"}),
                }]),
            },
        )
        .await;

        let turn2 = mock.read_turn().await;
        let id2 = turn2["id"].as_u64().unwrap();
        assert!(id2 > id1);

        let messages = turn2["params"]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "tool");
        assert_eq!(
            messages[0]["tool_call_id"], "tc-1",
            "tool_call_id must match the request"
        );
        let content_text = messages[0]["content"][0]["text"].as_str().unwrap();
        assert!(content_text.contains("tool_output"));
        assert!(
            messages[0].get("is_error").is_none() || messages[0]["is_error"].is_null(),
            "successful tool should not have is_error set"
        );

        mock.send_response(
            id2,
            &TurnResponse {
                stop_reason: StopReason::EndTurn,
                content: Some(ContentBlock::text_content("Done.")),
                tool_calls: None,
            },
        )
        .await;

        drop(mock);
        let _ = agent_handle.await;
    }

    #[tokio::test]
    async fn agent_respects_max_iterations() {
        let (agent_handle, mut mock, _tmp) = setup_agent("maxiter", 2).await;

        mock.send_human_message("Loop forever").await;
        let turn1 = mock.read_turn().await;
        let id1 = turn1["id"].as_u64().unwrap();

        mock.send_response(
            id1,
            &TurnResponse {
                stop_reason: StopReason::ToolUse,
                content: None,
                tool_calls: Some(vec![ToolCall {
                    id: "tc-1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "echo 1"}),
                }]),
            },
        )
        .await;

        let turn2 = mock.read_turn().await;
        let id2 = turn2["id"].as_u64().unwrap();

        mock.send_response(
            id2,
            &TurnResponse {
                stop_reason: StopReason::ToolUse,
                content: None,
                tool_calls: Some(vec![ToolCall {
                    id: "tc-2".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "echo 2"}),
                }]),
            },
        )
        .await;

        mock.send_human_message("After limit").await;
        let turn3 = mock.read_turn().await;
        assert_eq!(
            turn3["params"]["messages"][0]["content"][0]["text"],
            "After limit"
        );

        drop(mock);
        let _ = agent_handle.await;
    }

    #[tokio::test]
    async fn agent_handles_max_tokens() {
        let (agent_handle, mut mock, _tmp) = setup_agent("maxtok", 100).await;

        mock.send_human_message("Test").await;
        let turn1 = mock.read_turn().await;
        let id1 = turn1["id"].as_u64().unwrap();

        mock.send_response(
            id1,
            &TurnResponse {
                stop_reason: StopReason::MaxTokens,
                content: Some(ContentBlock::text_content("partial response...")),
                tool_calls: Some(vec![ToolCall {
                    id: "tc-incomplete".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                }]),
            },
        )
        .await;

        mock.send_human_message("Next").await;
        let turn2 = mock.read_turn().await;
        let messages = turn2["params"]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0]["role"], "user",
            "next turn should be a user message, not a tool result"
        );
        assert_eq!(messages[0]["content"][0]["text"], "Next");
        assert!(
            messages[0].get("tool_call_id").is_none() || messages[0]["tool_call_id"].is_null(),
            "next turn should not contain tool_call_id (would indicate tool results were sent)"
        );

        drop(mock);
        let _ = agent_handle.await;
    }

    #[tokio::test]
    async fn agent_sends_is_error_for_failing_tool() {
        let (agent_handle, mut mock, _tmp) = setup_agent("toolerr", 100).await;

        mock.send_human_message("Run failing command").await;
        let turn1 = mock.read_turn().await;
        let id1 = turn1["id"].as_u64().unwrap();

        mock.send_response(
            id1,
            &TurnResponse {
                stop_reason: StopReason::ToolUse,
                content: None,
                tool_calls: Some(vec![ToolCall {
                    id: "tc-fail".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "exit 1"}),
                }]),
            },
        )
        .await;

        let turn2 = mock.read_turn().await;
        let id2 = turn2["id"].as_u64().unwrap();
        let messages = turn2["params"]["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "tool");
        assert_eq!(messages[0]["tool_call_id"], "tc-fail");
        assert_eq!(
            messages[0]["is_error"], true,
            "failing tool should set is_error=true in the tool result message"
        );

        mock.send_response(
            id2,
            &TurnResponse {
                stop_reason: StopReason::EndTurn,
                content: Some(ContentBlock::text_content("Done.")),
                tool_calls: None,
            },
        )
        .await;

        drop(mock);
        let _ = agent_handle.await;
    }

    #[tokio::test]
    async fn discover_agents_returns_hashmap() {
        let tmp = tempfile::tempdir().unwrap();
        let research = tmp.path().join("research");
        let writer = tmp.path().join("writer");
        std::fs::create_dir(&research).unwrap();
        std::fs::create_dir(&writer).unwrap();
        std::fs::write(research.join("prompt.md"), "Research agent").unwrap();
        std::fs::write(writer.join("prompt.md"), "Writer agent").unwrap();

        let agents = discover_agents(tmp.path()).await.unwrap();
        assert_eq!(agents.len(), 2);
        assert_eq!(agents["research"], "Research agent");
        assert_eq!(agents["writer"], "Writer agent");
    }

    #[tokio::test]
    async fn discover_agents_skips_files() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("myagent");
        std::fs::create_dir(&agent).unwrap();
        std::fs::write(agent.join("prompt.md"), "Agent prompt").unwrap();
        std::fs::write(tmp.path().join("README.md"), "Not an agent").unwrap();

        let agents = discover_agents(tmp.path()).await.unwrap();
        assert_eq!(agents.len(), 1);
        assert!(agents.contains_key("myagent"));
    }

    #[tokio::test]
    async fn discover_agents_empty_dir_errors() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(discover_agents(tmp.path()).await.is_err());
    }

    #[test]
    fn parse_router_response_valid_agent() {
        let agents = HashMap::from([
            ("research".into(), "prompt".into()),
            ("writer".into(), "prompt".into()),
            ("router".into(), "prompt".into()),
        ]);
        let resp = TurnResponse {
            stop_reason: StopReason::EndTurn,
            content: Some(ContentBlock::text_content("research")),
            tool_calls: None,
        };
        assert_eq!(parse_router_response(&resp, &agents, "writer"), "research");
    }

    #[test]
    fn parse_router_response_trims_and_lowercases() {
        let agents = HashMap::from([
            ("research".into(), "prompt".into()),
            ("router".into(), "prompt".into()),
        ]);
        let resp = TurnResponse {
            stop_reason: StopReason::EndTurn,
            content: Some(ContentBlock::text_content("  Research \n")),
            tool_calls: None,
        };
        assert_eq!(parse_router_response(&resp, &agents, "writer"), "research");
    }

    #[test]
    fn parse_router_response_unknown_keeps_current() {
        let agents = HashMap::from([
            ("research".into(), "prompt".into()),
            ("router".into(), "prompt".into()),
        ]);
        let resp = TurnResponse {
            stop_reason: StopReason::EndTurn,
            content: Some(ContentBlock::text_content("nonexistent")),
            tool_calls: None,
        };
        assert_eq!(
            parse_router_response(&resp, &agents, "research"),
            "research"
        );
    }

    #[test]
    fn parse_router_response_rejects_router() {
        let agents = HashMap::from([
            ("research".into(), "prompt".into()),
            ("router".into(), "prompt".into()),
        ]);
        let resp = TurnResponse {
            stop_reason: StopReason::EndTurn,
            content: Some(ContentBlock::text_content("router")),
            tool_calls: None,
        };
        assert_eq!(
            parse_router_response(&resp, &agents, "research"),
            "research"
        );
    }

    #[test]
    fn parse_router_response_empty_keeps_current() {
        let agents = HashMap::from([("research".into(), "prompt".into())]);
        let resp = TurnResponse {
            stop_reason: StopReason::EndTurn,
            content: None,
            tool_calls: None,
        };
        assert_eq!(
            parse_router_response(&resp, &agents, "research"),
            "research"
        );
    }

    #[tokio::test]
    async fn single_agent_sets_agent_field() {
        let (agent_handle, mut mock, _tmp) = setup_agent("agentfield", 100).await;

        mock.send_human_message("Hello").await;
        let turn = mock.read_turn().await;

        assert_eq!(
            turn["params"]["agent"], "test",
            "single-agent turn must include agent name"
        );

        mock.send_response(
            turn["id"].as_u64().unwrap(),
            &TurnResponse {
                stop_reason: StopReason::EndTurn,
                content: Some(ContentBlock::text_content("Hi")),
                tool_calls: None,
            },
        )
        .await;

        drop(mock);
        let _ = agent_handle.await;
    }
}

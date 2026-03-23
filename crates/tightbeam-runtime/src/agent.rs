use crate::config::RuntimeConfig;
use crate::connection::DaemonConnection;
use crate::tools;
use tightbeam_protocol::{ContentBlock, Message, StopReason, TurnRequest};

pub(crate) async fn run_agent(config: RuntimeConfig) -> Result<(), String> {
    let system_prompt = tokio::fs::read_to_string(&config.system_prompt_path)
        .await
        .map_err(|e| {
            format!(
                "failed to read system prompt {}: {e}",
                config.system_prompt_path.display()
            )
        })?;

    let tool_defs = tools::tool_definitions(&config.tools);

    let mut conn = DaemonConnection::connect(&config.socket_path).await?;

    let mut first_turn = true;

    loop {
        let human = conn.wait_for_human_message().await?;

        let user_msg = Message {
            role: "user".into(),
            content: Some(human.content),
            tool_calls: None,
            tool_call_id: None,
            is_error: None,
        };

        let request = if first_turn {
            first_turn = false;
            TurnRequest {
                system: Some(system_prompt.clone()),
                tools: Some(tool_defs.clone()),
                messages: vec![user_msg],
            }
        } else {
            TurnRequest {
                system: None,
                tools: None,
                messages: vec![user_msg],
            }
        };

        let mut id = conn.send_turn(&request).await?;
        let mut iterations = 0u32;

        loop {
            let response = conn.read_turn_response(id).await?;

            match response.stop_reason {
                StopReason::EndTurn => break,
                StopReason::MaxTokens => {
                    eprintln!("tightbeam-runtime: max_tokens reached, ending turn");
                    break;
                }
                StopReason::ToolUse => {
                    iterations += 1;
                    if iterations >= config.max_iterations {
                        eprintln!(
                            "tightbeam-runtime: iteration limit ({}) reached, stopping",
                            config.max_iterations
                        );
                        break;
                    }

                    let tool_calls = match response.tool_calls {
                        Some(tc) if !tc.is_empty() => tc,
                        _ => break,
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
                        });
                    }

                    let turn = TurnRequest {
                        system: None,
                        tools: None,
                        messages: tool_result_messages,
                    };
                    id = conn.send_turn(&turn).await?;
                }
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
        let prompt_path = tmp.path().join("prompt.md");
        std::fs::write(&prompt_path, "You are a test agent.").unwrap();

        let sock_dir =
            std::env::temp_dir().join(format!("tb-agent-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&sock_dir);
        std::fs::create_dir_all(&sock_dir).unwrap();
        let sock_path = sock_dir.join("test.sock");

        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();

        let config = RuntimeConfig {
            system_prompt_path: prompt_path,
            tools: vec!["bash".into()],
            socket_path: sock_path,
            max_iterations,
            max_output_chars: 30000,
        };

        let agent_handle = tokio::spawn(async move { run_agent(config).await });

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
    async fn agent_sends_system_and_tools_on_first_turn() {
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

        // Send second human message — should NOT have system/tools
        mock.send_human_message("Second").await;
        let turn2 = mock.read_turn().await;

        assert!(turn2["params"].get("system").is_none() || turn2["params"]["system"].is_null());
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
}

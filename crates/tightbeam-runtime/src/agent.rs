use crate::config::RuntimeConfig;
use crate::connection::DaemonConnection;
use crate::tools;
use tightbeam_protocol::{Message, StopReason, TurnRequest};

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
            content: Some(serde_json::Value::String(human.content)),
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
                            content: Some(serde_json::Value::String(output)),
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
    use tightbeam_protocol::{ToolCall, TurnResponse};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    struct MockDaemon {
        reader: BufReader<tokio::net::unix::OwnedReadHalf>,
        writer: tokio::net::unix::OwnedWriteHalf,
    }

    impl MockDaemon {
        async fn read_turn(&mut self) -> serde_json::Value {
            let mut buf = String::new();
            self.reader.read_line(&mut buf).await.unwrap();
            serde_json::from_str(buf.trim()).unwrap()
        }

        async fn write_json(&mut self, value: &serde_json::Value) {
            let mut line = serde_json::to_string(value).unwrap();
            line.push('\n');
            self.writer.write_all(line.as_bytes()).await.unwrap();
            self.writer.flush().await.unwrap();
        }

        async fn send_human_message(&mut self, content: &str) {
            self.write_json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "human_message",
                "params": {"content": content}
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
        let (read, write) = stream.into_split();

        let mock = MockDaemon {
            reader: BufReader::new(read),
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
        // A2: positive assertion — first turn MUST include system and tools
        assert!(turn["params"]["system"].is_string(), "first turn must include system prompt");
        assert_eq!(turn["params"]["system"], "You are a test agent.");
        assert!(turn["params"]["tools"].is_array(), "first turn must include tools");
        assert!(!turn["params"]["tools"].as_array().unwrap().is_empty());
        assert_eq!(turn["params"]["messages"][0]["role"], "user");
        assert_eq!(turn["params"]["messages"][0]["content"], "Hello");

        // Respond with end_turn to complete the conversation
        mock.send_response(
            turn["id"].as_u64().unwrap(),
            &TurnResponse {
                stop_reason: StopReason::EndTurn,
                content: Some("Hi there!".into()),
                tool_calls: None,
            },
        )
        .await;

        // Send second human message — should NOT have system/tools
        mock.send_human_message("Second").await;
        let turn2 = mock.read_turn().await;

        assert!(turn2["params"].get("system").is_none() || turn2["params"]["system"].is_null());
        assert!(turn2["params"].get("tools").is_none() || turn2["params"]["tools"].is_null());
        assert_eq!(turn2["params"]["messages"][0]["content"], "Second");

        // Close by dropping mock → agent gets disconnect error
        drop(mock);
        let _ = agent_handle.await;
    }

    #[tokio::test]
    async fn agent_executes_tools_and_sends_results() {
        let (agent_handle, mut mock, _tmp) = setup_agent("toolexec", 100).await;

        mock.send_human_message("List files").await;
        let turn1 = mock.read_turn().await;
        let id1 = turn1["id"].as_u64().unwrap();

        // Respond with tool_use
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

        // Agent should execute tool and send results
        let turn2 = mock.read_turn().await;
        let id2 = turn2["id"].as_u64().unwrap();
        assert!(id2 > id1);

        let messages = turn2["params"]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "tool");
        // A1: verify tool_call_id matches exactly
        assert_eq!(messages[0]["tool_call_id"], "tc-1", "tool_call_id must match the request");
        let content = messages[0]["content"].as_str().unwrap();
        assert!(content.contains("tool_output"));
        // A1: verify is_error absent for successful tool execution
        assert!(
            messages[0].get("is_error").is_none() || messages[0]["is_error"].is_null(),
            "successful tool should not have is_error set"
        );

        // Now end the turn
        mock.send_response(
            id2,
            &TurnResponse {
                stop_reason: StopReason::EndTurn,
                content: Some("Done.".into()),
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

        // Iteration 1: tool_use
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

        // Iteration 2: tool_use again
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

        // Agent should stop here (max_iterations=2) and go back to waiting
        // for the next human message. It should NOT read or send another turn.
        // Send another human message to confirm it's waiting.
        mock.send_human_message("After limit").await;
        let turn3 = mock.read_turn().await;
        assert_eq!(turn3["params"]["messages"][0]["content"], "After limit");

        drop(mock);
        let _ = agent_handle.await;
    }

    #[tokio::test]
    async fn agent_handles_max_tokens() {
        let (agent_handle, mut mock, _tmp) = setup_agent("maxtok", 100).await;

        mock.send_human_message("Test").await;
        let turn1 = mock.read_turn().await;
        let id1 = turn1["id"].as_u64().unwrap();

        // Respond with max_tokens (should be treated as end_turn)
        mock.send_response(
            id1,
            &TurnResponse {
                stop_reason: StopReason::MaxTokens,
                content: Some("partial response...".into()),
                tool_calls: Some(vec![ToolCall {
                    id: "tc-incomplete".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                }]),
            },
        )
        .await;

        // A3: Agent should discard tool_calls and go back to waiting.
        // The NEXT thing the mock receives must be a turn from the next human message,
        // NOT a tool result turn. This proves tools were not executed.
        mock.send_human_message("Next").await;
        let turn2 = mock.read_turn().await;
        let messages = turn2["params"]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user", "next turn should be a user message, not a tool result");
        assert_eq!(messages[0]["content"], "Next");
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
                content: Some("Done.".into()),
                tool_calls: None,
            },
        )
        .await;

        drop(mock);
        let _ = agent_handle.await;
    }
}

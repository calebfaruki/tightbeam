use serde::Deserialize;
use std::path::Path;
use tightbeam_protocol::{HumanMessage, TurnRequest, TurnResponse};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

pub(crate) struct DaemonConnection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
}

#[derive(Deserialize)]
struct RpcResponse {
    id: Option<u64>,
    method: Option<String>,
    result: Option<serde_json::Value>,
    error: Option<RpcErrorPayload>,
    params: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct RpcErrorPayload {
    code: i32,
    message: String,
}

impl DaemonConnection {
    pub(crate) async fn connect(path: &Path) -> Result<Self, String> {
        let stream = UnixStream::connect(path)
            .await
            .map_err(|e| format!("cannot connect to daemon at {}: {e}", path.display()))?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
            next_id: 1,
        })
    }

    pub(crate) async fn send_turn(&mut self, request: &TurnRequest) -> Result<u64, String> {
        let id = self.next_id;
        self.next_id += 1;

        let rpc = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "turn",
            "params": request
        });

        let mut line = serde_json::to_string(&rpc).map_err(|e| format!("serialize error: {e}"))?;
        line.push('\n');

        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("write error: {e}"))?;
        self.writer
            .flush()
            .await
            .map_err(|e| format!("flush error: {e}"))?;

        Ok(id)
    }

    pub(crate) async fn read_turn_response(
        &mut self,
        expected_id: u64,
    ) -> Result<TurnResponse, String> {
        let mut line_buf = String::new();

        loop {
            line_buf.clear();
            match self.reader.read_line(&mut line_buf).await {
                Ok(0) => return Err("daemon disconnected".into()),
                Ok(_) => {}
                Err(e) => return Err(format!("read error: {e}")),
            }

            let trimmed = line_buf.trim();
            if trimmed.is_empty() {
                continue;
            }

            let resp: RpcResponse =
                serde_json::from_str(trimmed).map_err(|e| format!("invalid response JSON: {e}"))?;

            // Streaming notification (no id) — skip
            if resp.method.is_some() && resp.id.is_none() {
                continue;
            }

            // Final response (has id)
            if let Some(id) = resp.id {
                if id != expected_id {
                    return Err(format!(
                        "unexpected response id: expected {expected_id}, got {id}"
                    ));
                }

                if let Some(error) = resp.error {
                    return Err(format!("daemon error {}: {}", error.code, error.message));
                }

                let result = resp.result.ok_or("response missing result field")?;
                let turn_response: TurnResponse = serde_json::from_value(result)
                    .map_err(|e| format!("invalid TurnResponse: {e}"))?;

                return Ok(turn_response);
            }
        }
    }

    pub(crate) async fn wait_for_human_message(&mut self) -> Result<HumanMessage, String> {
        let mut line_buf = String::new();

        loop {
            line_buf.clear();
            match self.reader.read_line(&mut line_buf).await {
                Ok(0) => return Err("daemon disconnected".into()),
                Ok(_) => {}
                Err(e) => return Err(format!("read error: {e}")),
            }

            let trimmed = line_buf.trim();
            if trimmed.is_empty() {
                continue;
            }

            let resp: RpcResponse = serde_json::from_str(trimmed)
                .map_err(|e| format!("invalid JSON from daemon: {e}"))?;

            if resp.method.as_deref() == Some("human_message") {
                let params = resp.params.ok_or("human_message missing params")?;
                let msg: HumanMessage = serde_json::from_value(params)
                    .map_err(|e| format!("invalid HumanMessage: {e}"))?;
                return Ok(msg);
            }
        }
    }
}

#[cfg(test)]
mod connection_tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    async fn mock_socket_pair(name: &str) -> (DaemonConnection, tokio::net::UnixStream) {
        let sock_dir =
            std::env::temp_dir().join(format!("tb-conn-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&sock_dir);
        std::fs::create_dir_all(&sock_dir).unwrap();
        let sock_path = sock_dir.join("test.sock");

        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();

        let connect_fut = DaemonConnection::connect(&sock_path);
        let accept_fut = listener.accept();

        let (conn, accept_result) = tokio::join!(connect_fut, accept_fut);
        let conn = conn.unwrap();
        let (daemon_stream, _) = accept_result.unwrap();

        (conn, daemon_stream)
    }

    #[tokio::test]
    async fn send_turn_serializes_correctly() {
        let (mut conn, mut daemon) = mock_socket_pair("send").await;

        let request = TurnRequest {
            system: Some("You are helpful.".into()),
            tools: None,
            messages: vec![tightbeam_protocol::Message {
                role: "user".into(),
                content: Some(serde_json::Value::String("Hello".into())),
                tool_calls: None,
                tool_call_id: None,
                is_error: None,
            }],
        };

        let id = conn.send_turn(&request).await.unwrap();
        assert_eq!(id, 1);

        let mut buf = vec![0u8; 4096];
        let n = daemon.read(&mut buf).await.unwrap();
        let received = String::from_utf8_lossy(&buf[..n]);
        let parsed: serde_json::Value = serde_json::from_str(received.trim()).unwrap();

        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["method"], "turn");
        assert_eq!(parsed["params"]["system"], "You are helpful.");
    }

    #[tokio::test]
    async fn read_turn_response_skips_notifications() {
        let (mut conn, daemon) = mock_socket_pair("notif").await;
        let (_, mut daemon_writer) = daemon.into_split();

        // Notification has distinct text that must NOT appear in the returned response
        let notif = r#"{"jsonrpc":"2.0","method":"output","params":{"stream":"content","data":{"type":"text","text":"NOTIFICATION_TEXT"}}}"#;
        let final_resp = r#"{"jsonrpc":"2.0","id":1,"result":{"stop_reason":"end_turn","content":"RESPONSE_TEXT"}}"#;

        let payload = format!("{notif}\n{final_resp}\n");
        daemon_writer.write_all(payload.as_bytes()).await.unwrap();
        daemon_writer.flush().await.unwrap();

        let response = conn.read_turn_response(1).await.unwrap();
        assert!(matches!(
            response.stop_reason,
            tightbeam_protocol::StopReason::EndTurn
        ));
        assert_eq!(
            response.content.as_deref(),
            Some("RESPONSE_TEXT"),
            "should return response content, not notification content"
        );
    }

    #[tokio::test]
    async fn read_turn_response_with_tool_calls() {
        let (mut conn, daemon) = mock_socket_pair("tools").await;
        let (_, mut daemon_writer) = daemon.into_split();

        let resp = r#"{"jsonrpc":"2.0","id":1,"result":{"stop_reason":"tool_use","tool_calls":[{"id":"tc-1","name":"bash","input":{"command":"ls"}}]}}"#;
        daemon_writer
            .write_all(format!("{resp}\n").as_bytes())
            .await
            .unwrap();

        let response = conn.read_turn_response(1).await.unwrap();
        assert!(matches!(
            response.stop_reason,
            tightbeam_protocol::StopReason::ToolUse
        ));
        let tc = response.tool_calls.unwrap();
        assert_eq!(tc[0].name, "bash");
    }

    #[tokio::test]
    async fn read_turn_response_daemon_error() {
        let (mut conn, daemon) = mock_socket_pair("err").await;
        let (_, mut daemon_writer) = daemon.into_split();

        let resp = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"provider error"}}"#;
        daemon_writer
            .write_all(format!("{resp}\n").as_bytes())
            .await
            .unwrap();

        let err = conn.read_turn_response(1).await.unwrap_err();
        assert!(err.contains("provider error"));
    }

    #[tokio::test]
    async fn wait_for_human_message_blocks_and_returns() {
        let (mut conn, daemon) = mock_socket_pair("human").await;
        let (_, mut daemon_writer) = daemon.into_split();

        let msg =
            r#"{"jsonrpc":"2.0","method":"human_message","params":{"content":"Fix the bug."}}"#;
        daemon_writer
            .write_all(format!("{msg}\n").as_bytes())
            .await
            .unwrap();

        let human = conn.wait_for_human_message().await.unwrap();
        assert_eq!(human.content, "Fix the bug.");
    }

    #[tokio::test]
    async fn daemon_disconnect_returns_error() {
        let (mut conn, daemon) = mock_socket_pair("dc").await;
        drop(daemon);

        let err = conn.wait_for_human_message().await.unwrap_err();
        assert!(err.contains("disconnected"));
    }

    #[tokio::test]
    async fn sequential_ids_increment() {
        let (mut conn, _daemon) = mock_socket_pair("ids").await;

        let request = TurnRequest {
            system: None,
            tools: None,
            messages: vec![],
        };

        let id1 = conn.send_turn(&request).await.unwrap();
        let id2 = conn.send_turn(&request).await.unwrap();
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[tokio::test]
    async fn read_turn_response_rejects_id_mismatch() {
        let (mut conn, daemon) = mock_socket_pair("idmis").await;
        let (_, mut daemon_writer) = daemon.into_split();

        let resp =
            r#"{"jsonrpc":"2.0","id":99,"result":{"stop_reason":"end_turn","content":"wrong id"}}"#;
        daemon_writer
            .write_all(format!("{resp}\n").as_bytes())
            .await
            .unwrap();

        let err = conn.read_turn_response(1).await.unwrap_err();
        assert!(
            err.contains("unexpected response id"),
            "should reject mismatched id, got: {err}"
        );
    }

    #[tokio::test]
    async fn wait_for_human_message_skips_other_notifications() {
        let (mut conn, daemon) = mock_socket_pair("skipnotif").await;
        let (_, mut daemon_writer) = daemon.into_split();

        // Send an "output" notification first, then a human_message
        let output_notif = r#"{"jsonrpc":"2.0","method":"output","params":{"stream":"content","data":{"type":"text","text":"noise"}}}"#;
        let human_msg =
            r#"{"jsonrpc":"2.0","method":"human_message","params":{"content":"Real message"}}"#;

        let payload = format!("{output_notif}\n{human_msg}\n");
        daemon_writer.write_all(payload.as_bytes()).await.unwrap();
        daemon_writer.flush().await.unwrap();

        let human = conn.wait_for_human_message().await.unwrap();
        assert_eq!(
            human.content, "Real message",
            "should skip output notification and return human_message"
        );
    }
}

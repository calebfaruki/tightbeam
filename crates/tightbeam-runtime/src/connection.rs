use serde::Deserialize;
use std::path::Path;
use tightbeam_protocol::framing::{read_frame, write_frame};
use tightbeam_protocol::{HumanMessage, TurnRequest, TurnResponse};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

pub(crate) struct DaemonConnection {
    reader: OwnedReadHalf,
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
        let mut conn = Self {
            reader,
            writer,
            next_id: 1,
        };

        let register = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "register",
            "params": {"role": "runtime"}
        });
        let payload = serde_json::to_vec(&register).map_err(|e| format!("serialize error: {e}"))?;
        write_frame(&mut conn.writer, &payload)
            .await
            .map_err(|e| format!("write error: {e}"))?;

        Ok(conn)
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

        let payload = serde_json::to_vec(&rpc).map_err(|e| format!("serialize error: {e}"))?;
        write_frame(&mut self.writer, &payload)
            .await
            .map_err(|e| format!("write error: {e}"))?;

        Ok(id)
    }

    async fn read_response(&mut self) -> Result<RpcResponse, String> {
        let payload = match read_frame(&mut self.reader).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Err("daemon disconnected".into()),
            Err(e) => return Err(format!("read error: {e}")),
        };
        let raw = String::from_utf8_lossy(&payload);
        serde_json::from_str(&raw).map_err(|e| format!("invalid JSON from daemon: {e}"))
    }

    pub(crate) async fn read_turn_response(
        &mut self,
        expected_id: u64,
    ) -> Result<TurnResponse, String> {
        loop {
            let resp = self.read_response().await?;

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
        loop {
            let resp = self.read_response().await?;

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
    use tightbeam_protocol::framing::{read_frame, write_frame};
    use tokio::io::AsyncWriteExt;

    async fn mock_socket_pair(
        name: &str,
    ) -> (
        DaemonConnection,
        tokio::net::unix::OwnedReadHalf,
        tokio::net::unix::OwnedWriteHalf,
    ) {
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
        let (mut daemon_reader, daemon_writer) = daemon_stream.into_split();

        // Consume the register frame sent by connect()
        let frame = read_frame(&mut daemon_reader).await.unwrap().unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(parsed["method"], "register");

        (conn, daemon_reader, daemon_writer)
    }

    async fn write_json_frame(writer: &mut (impl AsyncWriteExt + Unpin), json: &str) {
        write_frame(writer, json.as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn send_turn_serializes_correctly() {
        let (mut conn, mut daemon_reader, _) = mock_socket_pair("send").await;

        let request = TurnRequest {
            system: Some("You are helpful.".into()),
            tools: None,
            messages: vec![tightbeam_protocol::Message {
                role: "user".into(),
                content: Some(tightbeam_protocol::ContentBlock::text_content("Hello")),
                tool_calls: None,
                tool_call_id: None,
                is_error: None,
                agent: None,
            }],
            agent: None,
        };

        let id = conn.send_turn(&request).await.unwrap();
        assert_eq!(id, 1);

        let frame = read_frame(&mut daemon_reader).await.unwrap().unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&frame).unwrap();

        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["method"], "turn");
        assert_eq!(parsed["params"]["system"], "You are helpful.");
        assert_eq!(
            parsed["params"]["messages"][0]["content"][0]["text"],
            "Hello"
        );
    }

    #[tokio::test]
    async fn read_turn_response_skips_notifications() {
        let (mut conn, _, mut daemon_writer) = mock_socket_pair("notif").await;

        let notif = r#"{"jsonrpc":"2.0","method":"output","params":{"stream":"content","data":{"type":"text","text":"NOTIFICATION_TEXT"}}}"#;
        let final_resp = r#"{"jsonrpc":"2.0","id":1,"result":{"stop_reason":"end_turn","content":[{"type":"text","text":"RESPONSE_TEXT"}]}}"#;

        write_json_frame(&mut daemon_writer, notif).await;
        write_json_frame(&mut daemon_writer, final_resp).await;

        let response = conn.read_turn_response(1).await.unwrap();
        assert!(matches!(
            response.stop_reason,
            tightbeam_protocol::StopReason::EndTurn
        ));
        assert_eq!(
            tightbeam_protocol::content_text(&response.content),
            Some("RESPONSE_TEXT"),
            "should return response content, not notification content"
        );
    }

    #[tokio::test]
    async fn read_turn_response_with_tool_calls() {
        let (mut conn, _, mut daemon_writer) = mock_socket_pair("tools").await;

        let resp = r#"{"jsonrpc":"2.0","id":1,"result":{"stop_reason":"tool_use","tool_calls":[{"id":"tc-1","name":"bash","input":{"command":"ls"}}]}}"#;
        write_json_frame(&mut daemon_writer, resp).await;

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
        let (mut conn, _, mut daemon_writer) = mock_socket_pair("err").await;

        let resp = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"provider error"}}"#;
        write_json_frame(&mut daemon_writer, resp).await;

        let err = conn.read_turn_response(1).await.unwrap_err();
        assert!(err.contains("provider error"));
    }

    #[tokio::test]
    async fn wait_for_human_message_blocks_and_returns() {
        let (mut conn, _, mut daemon_writer) = mock_socket_pair("human").await;

        let msg = r#"{"jsonrpc":"2.0","method":"human_message","params":{"content":[{"type":"text","text":"Fix the bug."}]}}"#;
        write_json_frame(&mut daemon_writer, msg).await;

        let human = conn.wait_for_human_message().await.unwrap();
        assert_eq!(
            tightbeam_protocol::content_text(&Some(human.content)),
            Some("Fix the bug.")
        );
    }

    #[tokio::test]
    async fn daemon_disconnect_returns_error() {
        let (mut conn, daemon_reader, daemon_writer) = mock_socket_pair("dc").await;
        drop(daemon_reader);
        drop(daemon_writer);

        let err = conn.wait_for_human_message().await.unwrap_err();
        assert!(err.contains("disconnected"));
    }

    #[tokio::test]
    async fn sequential_ids_increment() {
        let (mut conn, _reader, _writer) = mock_socket_pair("ids").await;

        let request = TurnRequest {
            system: None,
            tools: None,
            messages: vec![],
            agent: None,
        };

        let id1 = conn.send_turn(&request).await.unwrap();
        let id2 = conn.send_turn(&request).await.unwrap();
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[tokio::test]
    async fn read_turn_response_rejects_id_mismatch() {
        let (mut conn, _, mut daemon_writer) = mock_socket_pair("idmis").await;

        let resp = r#"{"jsonrpc":"2.0","id":99,"result":{"stop_reason":"end_turn","content":[{"type":"text","text":"wrong id"}]}}"#;
        write_json_frame(&mut daemon_writer, resp).await;

        let err = conn.read_turn_response(1).await.unwrap_err();
        assert!(
            err.contains("unexpected response id"),
            "should reject mismatched id, got: {err}"
        );
    }

    #[tokio::test]
    async fn wait_for_human_message_skips_other_notifications() {
        let (mut conn, _, mut daemon_writer) = mock_socket_pair("skipnotif").await;

        let output_notif = r#"{"jsonrpc":"2.0","method":"output","params":{"stream":"content","data":{"type":"text","text":"noise"}}}"#;
        let human_msg = r#"{"jsonrpc":"2.0","method":"human_message","params":{"content":[{"type":"text","text":"Real message"}]}}"#;

        write_json_frame(&mut daemon_writer, output_notif).await;
        write_json_frame(&mut daemon_writer, human_msg).await;

        let human = conn.wait_for_human_message().await.unwrap();
        assert_eq!(
            tightbeam_protocol::content_text(&Some(human.content)),
            Some("Real message"),
            "should skip output notification and return human_message"
        );
    }
}

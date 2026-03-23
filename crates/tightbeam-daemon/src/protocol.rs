use serde::{Deserialize, Serialize};

pub use tightbeam_protocol::{
    ContentBlock, Message, StopReason, StreamData, ToolCall, ToolDefinition, TurnRequest,
    TurnResponse,
};
pub use tightbeam_protocol::{content_text, framing};

// --- Inbound requests (container → tightbeam) ---

#[derive(Debug, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: Option<u64>,
    pub method: String,
    pub params: Option<serde_json::Value>,
}

// --- Outbound responses (tightbeam → container) ---

#[derive(Debug, Serialize)]
pub struct StreamNotification {
    pub jsonrpc: &'static str,
    pub method: &'static str,
    pub params: StreamParams,
}

#[derive(Debug, Serialize)]
pub struct StreamParams {
    pub stream: String,
    pub data: StreamData,
}

#[derive(Debug, Serialize)]
pub struct FinalResponse {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub result: FinalResult,
}

#[derive(Debug, Serialize)]
pub struct FinalResult {
    pub stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ContentBlock>>,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub error: RpcError,
}

#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

// --- Validation ---

pub enum ValidatedRequest {
    Turn { id: u64, params: TurnRequest },
}

pub fn validate_request(raw: &str) -> Result<ValidatedRequest, ErrorResponse> {
    let request: RpcRequest = serde_json::from_str(raw.trim()).map_err(|e| ErrorResponse {
        jsonrpc: "2.0",
        id: 0,
        error: RpcError {
            code: -32700,
            message: format!("parse error: {e}"),
        },
    })?;

    let id = request.id.unwrap_or(0);

    if request.jsonrpc != "2.0" {
        return Err(build_error(id, -32600, "invalid jsonrpc version".into()));
    }

    let params_value = request
        .params
        .ok_or_else(|| build_error(id, -32600, "missing params".into()))?;

    match request.method.as_str() {
        "turn" => {
            let params: TurnRequest = serde_json::from_value(params_value)
                .map_err(|e| build_error(id, -32602, format!("invalid turn params: {e}")))?;
            Ok(ValidatedRequest::Turn { id, params })
        }
        "send_message" | "mcp_call" => Err(build_error(
            id,
            -32601,
            format!("method '{}' not implemented (v2)", request.method),
        )),
        _ => Err(build_error(
            id,
            -32601,
            format!("unknown method: {}", request.method),
        )),
    }
}

// --- Builders ---

pub fn build_error(id: u64, code: i32, message: String) -> ErrorResponse {
    ErrorResponse {
        jsonrpc: "2.0",
        id,
        error: RpcError { code, message },
    }
}

pub fn build_notification(stream: &str, data: StreamData) -> StreamNotification {
    StreamNotification {
        jsonrpc: "2.0",
        method: "output",
        params: StreamParams {
            stream: stream.to_string(),
            data,
        },
    }
}

pub fn build_final_response(
    id: u64,
    stop_reason: StopReason,
    tool_calls: Option<Vec<ToolCall>>,
    content: Option<Vec<ContentBlock>>,
) -> FinalResponse {
    FinalResponse {
        jsonrpc: "2.0",
        id,
        result: FinalResult {
            stop_reason,
            tool_calls,
            content,
        },
    }
}

#[cfg(test)]
mod protocol_parsing {
    use super::*;

    #[test]
    fn valid_turn_parses() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"messages":[{"role":"user","content":[{"type":"text","text":"Hello"}]}]}}"#;
        let Ok(ValidatedRequest::Turn { id, params }) = validate_request(raw) else {
            panic!("expected Turn");
        };
        assert_eq!(id, 1);
        assert_eq!(params.messages.len(), 1);
        assert_eq!(params.messages[0].role, "user");
        assert!(params.system.is_none());
        assert!(params.tools.is_none());
    }

    #[test]
    fn turn_with_system_and_tools_parses() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"method":"turn","params":{"system":"You are helpful","tools":[{"name":"bash","description":"Run a command","parameters":{"type":"object"}}],"messages":[{"role":"user","content":[{"type":"text","text":"Hi"}]}]}}"#;
        let Ok(ValidatedRequest::Turn { id, params }) = validate_request(raw) else {
            panic!("expected Turn");
        };
        assert_eq!(id, 1);
        assert_eq!(params.system.as_deref(), Some("You are helpful"));
        assert_eq!(params.tools.as_ref().unwrap().len(), 1);
        assert_eq!(params.tools.as_ref().unwrap()[0].name, "bash");
    }

    #[test]
    fn turn_with_tool_results_parses() {
        let raw = r#"{"jsonrpc":"2.0","id":2,"method":"turn","params":{"messages":[{"role":"tool","tool_call_id":"tc-001","content":[{"type":"text","text":"output here"}]}]}}"#;
        let Ok(ValidatedRequest::Turn { id, params }) = validate_request(raw) else {
            panic!("expected Turn");
        };
        assert_eq!(id, 2);
        assert_eq!(params.messages[0].role, "tool");
        assert_eq!(params.messages[0].tool_call_id.as_deref(), Some("tc-001"));
        assert_eq!(
            content_text(&params.messages[0].content),
            Some("output here")
        );
    }

    #[test]
    fn future_methods_return_not_implemented() {
        let raw = r#"{"jsonrpc":"2.0","id":3,"method":"send_message","params":{"text":"hello"}}"#;
        match validate_request(raw) {
            Err(e) => {
                assert_eq!(e.error.code, -32601);
                assert!(e.error.message.contains("not implemented"));
            }
            _ => panic!("expected error"),
        }

        let raw = r#"{"jsonrpc":"2.0","id":4,"method":"mcp_call","params":{"server":"github"}}"#;
        match validate_request(raw) {
            Err(e) => {
                assert_eq!(e.error.code, -32601);
                assert!(e.error.message.contains("not implemented"));
            }
            _ => panic!("expected error"),
        }
    }

    #[test]
    fn unknown_method_returns_error() {
        let raw = r#"{"jsonrpc":"2.0","id":5,"method":"bogus","params":{}}"#;
        match validate_request(raw) {
            Err(e) => {
                assert_eq!(e.error.code, -32601);
                assert!(e.error.message.contains("unknown method"));
            }
            _ => panic!("expected error"),
        }
    }

    #[test]
    fn invalid_json_returns_parse_error() {
        match validate_request("not json") {
            Err(e) => assert_eq!(e.error.code, -32700),
            _ => panic!("expected parse error"),
        }
    }

    #[test]
    fn wrong_jsonrpc_version_returns_error() {
        let raw = r#"{"jsonrpc":"1.0","id":1,"method":"turn","params":{"messages":[]}}"#;
        match validate_request(raw) {
            Err(e) => assert_eq!(e.error.code, -32600),
            _ => panic!("expected invalid version error"),
        }
    }

    #[test]
    fn missing_params_returns_error() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"method":"turn"}"#;
        match validate_request(raw) {
            Err(e) => {
                assert_eq!(e.error.code, -32600);
                assert!(e.error.message.contains("missing params"));
            }
            _ => panic!("expected missing params error"),
        }
    }

    #[test]
    fn notification_serializes_correctly() {
        let notif = build_notification(
            "content",
            StreamData {
                data_type: "text".into(),
                text: Some("hello".into()),
                id: None,
                name: None,
                input: None,
            },
        );
        let json = serde_json::to_string(&notif).unwrap();
        assert!(json.contains("\"method\":\"output\""));
        assert!(json.contains("\"stream\":\"content\""));
        assert!(json.contains("\"text\":\"hello\""));
        assert!(!json.contains("\"id\"")); // no id field on notifications
    }

    #[test]
    fn final_response_serializes_correctly() {
        let resp = build_final_response(
            1,
            StopReason::EndTurn,
            None,
            Some(ContentBlock::text_content("The answer is 42.")),
        );
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"id\":1"));
        assert!(json.contains("\"stop_reason\":\"end_turn\""));
        assert!(json.contains("The answer is 42."));
    }

    #[test]
    fn final_response_with_tool_calls() {
        let resp = build_final_response(
            1,
            StopReason::ToolUse,
            Some(vec![ToolCall {
                id: "tc-001".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            }]),
            None,
        );
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"stop_reason\":\"tool_use\""));
        assert!(json.contains("\"tc-001\""));
        assert!(json.contains("\"bash\""));
    }

    #[test]
    fn error_response_serializes() {
        let err = build_error(1, 429, "Rate limit exceeded".into());
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("\"code\":429"));
        assert!(json.contains("Rate limit exceeded"));
    }
}

use serde::{Deserialize, Serialize};

// --- Shared types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

// --- Inbound requests (container → tightbeam) ---

#[derive(Debug, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: Option<u64>,
    pub method: String,
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct LlmCallParams {
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    pub system: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ToolResultParams {
    pub tool_call_id: String,
    pub result: String,
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

#[derive(Debug, Clone, Serialize)]
pub struct StreamData {
    #[serde(rename = "type")]
    pub data_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct FinalResponse {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub result: FinalResult,
}

#[derive(Debug, Serialize)]
pub struct FinalResult {
    pub stop_reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
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
    LlmCall { id: u64, params: LlmCallParams },
    ToolResult { id: u64, params: ToolResultParams },
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
        "llm_call" => {
            let params: LlmCallParams = serde_json::from_value(params_value)
                .map_err(|e| build_error(id, -32602, format!("invalid llm_call params: {e}")))?;
            Ok(ValidatedRequest::LlmCall { id, params })
        }
        "tool_result" => {
            let params: ToolResultParams = serde_json::from_value(params_value)
                .map_err(|e| build_error(id, -32602, format!("invalid tool_result params: {e}")))?;
            Ok(ValidatedRequest::ToolResult { id, params })
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
    stop_reason: String,
    tool_calls: Option<Vec<ToolCall>>,
    text: Option<String>,
) -> FinalResponse {
    FinalResponse {
        jsonrpc: "2.0",
        id,
        result: FinalResult {
            stop_reason,
            tool_calls,
            text,
        },
    }
}

pub fn send_line(line: &str) -> Vec<u8> {
    let mut buf = line.as_bytes().to_vec();
    buf.push(b'\n');
    buf
}

#[cfg(test)]
mod protocol_parsing {
    use super::*;

    #[test]
    fn valid_llm_call_parses() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"method":"llm_call","params":{"messages":[{"role":"user","content":"Hello"}],"tools":[]}}"#;
        match validate_request(raw) {
            Ok(ValidatedRequest::LlmCall { id, params }) => {
                assert_eq!(id, 1);
                assert_eq!(params.messages.len(), 1);
                assert_eq!(params.messages[0].role, "user");
            }
            _ => panic!("expected LlmCall"),
        }
    }

    #[test]
    fn valid_tool_result_parses() {
        let raw = r#"{"jsonrpc":"2.0","id":2,"method":"tool_result","params":{"tool_call_id":"tc-001","result":"output here"}}"#;
        match validate_request(raw) {
            Ok(ValidatedRequest::ToolResult { id, params }) => {
                assert_eq!(id, 2);
                assert_eq!(params.tool_call_id, "tc-001");
                assert_eq!(params.result, "output here");
            }
            _ => panic!("expected ToolResult"),
        }
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
        let raw = r#"{"jsonrpc":"1.0","id":1,"method":"llm_call","params":{"messages":[]}}"#;
        match validate_request(raw) {
            Err(e) => assert_eq!(e.error.code, -32600),
            _ => panic!("expected invalid version error"),
        }
    }

    #[test]
    fn missing_params_returns_error() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"method":"llm_call"}"#;
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
        let resp =
            build_final_response(1, "end_turn".into(), None, Some("The answer is 42.".into()));
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"id\":1"));
        assert!(json.contains("\"stop_reason\":\"end_turn\""));
        assert!(json.contains("The answer is 42."));
    }

    #[test]
    fn final_response_with_tool_calls() {
        let resp = build_final_response(
            1,
            "tool_use".into(),
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

    #[test]
    fn send_line_appends_newline() {
        let line = r#"{"jsonrpc":"2.0"}"#;
        let result = send_line(line);
        assert_eq!(result.last(), Some(&b'\n'));
        assert_eq!(result.len(), line.len() + 1);
    }
}

pub mod framing;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "file_incoming")]
    FileIncoming {
        filename: String,
        mime_type: String,
        size: u64,
    },
    #[serde(rename = "image")]
    Image { media_type: String, data: String },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }

    pub fn text_content(s: impl Into<String>) -> Vec<ContentBlock> {
        vec![Self::text(s)]
    }

    pub fn file_incoming(
        filename: impl Into<String>,
        mime_type: impl Into<String>,
        size: u64,
    ) -> Self {
        Self::FileIncoming {
            filename: filename.into(),
            mime_type: mime_type.into(),
            size,
        }
    }

    pub fn image(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self::Image {
            media_type: media_type.into(),
            data: data.into(),
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }
}

pub fn file_incoming_indices(blocks: &[ContentBlock]) -> Vec<usize> {
    blocks
        .iter()
        .enumerate()
        .filter_map(|(i, b)| matches!(b, ContentBlock::FileIncoming { .. }).then_some(i))
        .collect()
}

pub fn is_supported_image(mime_type: &str) -> bool {
    matches!(
        mime_type.to_ascii_lowercase().as_str(),
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

pub fn content_text(blocks: &Option<Vec<ContentBlock>>) -> Option<&str> {
    blocks
        .as_ref()
        .and_then(|b| b.first())
        .and_then(|b| b.as_text())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ContentBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}

impl StopReason {
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "end_turn" => Self::EndTurn,
            "tool_use" => Self::ToolUse,
            "max_tokens" => Self::MaxTokens,
            _ => Self::EndTurn,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnResponse {
    pub stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ContentBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HumanMessage {
    pub content: Vec<ContentBlock>,
}

#[cfg(test)]
mod serialization {
    use super::*;

    #[test]
    fn content_block_text_serializes() {
        let block = ContentBlock::text("hello");
        let json = serde_json::to_string(&block).unwrap();
        assert_eq!(json, r#"{"type":"text","text":"hello"}"#);

        let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.as_text(), Some("hello"));
    }

    #[test]
    fn content_text_helper() {
        let blocks = Some(ContentBlock::text_content("hello"));
        assert_eq!(content_text(&blocks), Some("hello"));

        let none: Option<Vec<ContentBlock>> = None;
        assert_eq!(content_text(&none), None);

        let empty: Option<Vec<ContentBlock>> = Some(vec![]);
        assert_eq!(content_text(&empty), None);
    }

    #[test]
    fn message_round_trips() {
        let msg = Message {
            role: "user".into(),
            content: Some(ContentBlock::text_content("hello")),
            tool_calls: None,
            tool_call_id: None,
            is_error: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("tool_calls"));
        assert!(!json.contains("is_error"));
        assert!(json.contains(r#""content":[{"type":"text","text":"hello"}]"#));

        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.role, "user");
        assert_eq!(content_text(&parsed.content), Some("hello"));
        assert!(parsed.tool_calls.is_none());
    }

    #[test]
    fn message_with_tool_calls_round_trips() {
        let msg = Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "tc-1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            }]),
            tool_call_id: None,
            is_error: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();
        let tc = parsed.tool_calls.unwrap();
        assert_eq!(tc[0].name, "bash");
    }

    #[test]
    fn tool_message_with_is_error() {
        let msg = Message {
            role: "tool".into(),
            content: Some(ContentBlock::text_content("file not found")),
            tool_calls: None,
            tool_call_id: Some("tc-1".into()),
            is_error: Some(true),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"is_error\":true"));

        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.is_error, Some(true));
    }

    #[test]
    fn tool_definition_round_trips() {
        let td = ToolDefinition {
            name: "bash".into(),
            description: "Run a shell command".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"]
            }),
        };
        let json = serde_json::to_string(&td).unwrap();
        assert!(json.contains("\"parameters\""));
        assert!(!json.contains("input_schema"));

        let parsed: ToolDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "bash");
    }

    #[test]
    fn stop_reason_serializes_to_snake_case() {
        assert_eq!(
            serde_json::to_string(&StopReason::EndTurn).unwrap(),
            "\"end_turn\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::ToolUse).unwrap(),
            "\"tool_use\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::MaxTokens).unwrap(),
            "\"max_tokens\""
        );
    }

    #[test]
    fn stop_reason_deserializes_from_snake_case() {
        let end: StopReason = serde_json::from_str("\"end_turn\"").unwrap();
        assert!(matches!(end, StopReason::EndTurn));

        let tool: StopReason = serde_json::from_str("\"tool_use\"").unwrap();
        assert!(matches!(tool, StopReason::ToolUse));

        let max: StopReason = serde_json::from_str("\"max_tokens\"").unwrap();
        assert!(matches!(max, StopReason::MaxTokens));
    }

    #[test]
    fn stop_reason_from_str_lossy() {
        assert!(matches!(
            StopReason::from_str_lossy("end_turn"),
            StopReason::EndTurn
        ));
        assert!(matches!(
            StopReason::from_str_lossy("tool_use"),
            StopReason::ToolUse
        ));
        assert!(matches!(
            StopReason::from_str_lossy("unknown"),
            StopReason::EndTurn
        ));
    }

    #[test]
    fn turn_request_omits_none_fields() {
        let req = TurnRequest {
            system: None,
            tools: None,
            messages: vec![Message {
                role: "tool".into(),
                content: Some(ContentBlock::text_content("output")),
                tool_calls: None,
                tool_call_id: Some("tc-1".into()),
                is_error: None,
            }],
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("\"system\""));
        assert!(!json.contains("\"tools\""));
        assert!(json.contains("\"messages\""));
    }

    #[test]
    fn turn_request_with_all_fields() {
        let req = TurnRequest {
            system: Some("You are helpful.".into()),
            tools: Some(vec![ToolDefinition {
                name: "bash".into(),
                description: "Run a command".into(),
                parameters: serde_json::json!({"type": "object"}),
            }]),
            messages: vec![Message {
                role: "user".into(),
                content: Some(ContentBlock::text_content("hi")),
                tool_calls: None,
                tool_call_id: None,
                is_error: None,
            }],
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: TurnRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.system.as_deref(), Some("You are helpful."));
        assert_eq!(parsed.tools.unwrap().len(), 1);
        assert_eq!(parsed.messages.len(), 1);
    }

    #[test]
    fn turn_response_round_trips() {
        let resp = TurnResponse {
            stop_reason: StopReason::ToolUse,
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "tc-1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            }]),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"stop_reason\":\"tool_use\""));
        assert!(!json.contains("\"content\""));

        let parsed: TurnResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed.stop_reason, StopReason::ToolUse));
        assert_eq!(parsed.tool_calls.unwrap()[0].name, "bash");
    }

    #[test]
    fn turn_response_end_turn_with_content() {
        let resp = TurnResponse {
            stop_reason: StopReason::EndTurn,
            content: Some(ContentBlock::text_content("The answer is 42.")),
            tool_calls: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: TurnResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed.stop_reason, StopReason::EndTurn));
        assert_eq!(content_text(&parsed.content), Some("The answer is 42."));
        assert!(parsed.tool_calls.is_none());
    }

    #[test]
    fn stream_data_text_delta() {
        let data = StreamData {
            data_type: "text".into(),
            text: Some("Hello".into()),
            id: None,
            name: None,
            input: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("\"type\":\"text\""));
        assert!(json.contains("\"text\":\"Hello\""));
        assert!(!json.contains("\"id\""));
    }

    #[test]
    fn stream_data_tool_use_start() {
        let data = StreamData {
            data_type: "tool_use_start".into(),
            text: None,
            id: Some("tc-1".into()),
            name: Some("bash".into()),
            input: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("\"type\":\"tool_use_start\""));
        assert!(json.contains("\"id\":\"tc-1\""));
        assert!(json.contains("\"name\":\"bash\""));
        assert!(!json.contains("\"text\""));
    }

    #[test]
    fn human_message_round_trips() {
        let msg = HumanMessage {
            content: ContentBlock::text_content("Fix the login bug."),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: HumanMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(
            content_text(&Some(parsed.content)),
            Some("Fix the login bug.")
        );
    }

    #[test]
    fn message_none_content_omitted() {
        let msg = Message {
            role: "assistant".into(),
            content: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("content"));
    }

    #[test]
    fn plain_string_content_rejected() {
        let json = r#"{"role":"user","content":"plain string"}"#;
        let result: Result<Message, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "old-format plain string content must be rejected"
        );
    }

    #[test]
    fn file_incoming_round_trips() {
        let block = ContentBlock::file_incoming("photo.png", "image/png", 1024);
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains(r#""type":"file_incoming""#));
        assert!(json.contains(r#""filename":"photo.png""#));
        assert!(json.contains(r#""mime_type":"image/png""#));
        assert!(json.contains(r#""size":1024"#));

        let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, block);
    }

    #[test]
    fn image_round_trips() {
        let block = ContentBlock::image("image/png", "iVBOR...");
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains(r#""type":"image""#));
        assert!(json.contains(r#""media_type":"image/png""#));
        assert!(json.contains(r#""data":"iVBOR...""#));

        let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, block);
    }

    #[test]
    fn as_text_returns_none_for_non_text() {
        let img = ContentBlock::image("image/png", "data");
        assert_eq!(img.as_text(), None);

        let fi = ContentBlock::file_incoming("f.png", "image/png", 1);
        assert_eq!(fi.as_text(), None);
    }

    #[test]
    fn file_incoming_indices_finds_correct_positions() {
        let blocks = vec![
            ContentBlock::text("hello"),
            ContentBlock::file_incoming("a.png", "image/png", 100),
            ContentBlock::text("world"),
            ContentBlock::file_incoming("b.jpg", "image/jpeg", 200),
        ];
        assert_eq!(file_incoming_indices(&blocks), vec![1, 3]);
    }

    #[test]
    fn file_incoming_indices_empty_when_none() {
        let blocks = vec![ContentBlock::text("hello")];
        assert!(file_incoming_indices(&blocks).is_empty());
        assert!(file_incoming_indices(&[]).is_empty());
    }

    #[test]
    fn is_supported_image_accepts_valid_types() {
        assert!(is_supported_image("image/png"));
        assert!(is_supported_image("image/jpeg"));
        assert!(is_supported_image("image/gif"));
        assert!(is_supported_image("image/webp"));
    }

    #[test]
    fn is_supported_image_case_insensitive() {
        assert!(is_supported_image("Image/PNG"));
        assert!(is_supported_image("IMAGE/JPEG"));
    }

    #[test]
    fn is_supported_image_rejects_non_images() {
        assert!(!is_supported_image("application/pdf"));
        assert!(!is_supported_image("text/plain"));
        assert!(!is_supported_image("image/svg+xml"));
        assert!(!is_supported_image(""));
    }
}

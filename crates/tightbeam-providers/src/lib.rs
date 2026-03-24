pub mod claude;

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tightbeam_protocol::{Message, StreamData, ToolCall, ToolDefinition};

#[derive(Debug, Clone)]
pub enum StreamEvent {
    ContentDelta { text: String },
    ToolUseStart { id: String, name: String },
    ToolUseInput { json: String },
    Done { stop_reason: String },
}

impl StreamEvent {
    pub fn to_stream_data(&self) -> Option<StreamData> {
        match self {
            Self::ContentDelta { text } => Some(StreamData {
                data_type: "text".into(),
                text: Some(text.clone()),
                id: None,
                name: None,
                input: None,
            }),
            Self::ToolUseStart { id, name } => Some(StreamData {
                data_type: "tool_use_start".into(),
                text: None,
                id: Some(id.clone()),
                name: Some(name.clone()),
                input: None,
            }),
            Self::ToolUseInput { json } => Some(StreamData {
                data_type: "tool_use_input".into(),
                text: Some(json.clone()),
                id: None,
                name: None,
                input: None,
            }),
            Self::Done { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Anthropic,
}

impl Provider {
    pub fn build(&self) -> Box<dyn LlmProvider> {
        match self {
            Self::Anthropic => Box::new(claude::ClaudeProvider::new()),
        }
    }
}

pub struct ProviderConfig {
    pub model: String,
    pub api_key: String,
    pub max_tokens: u32,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn call(
        &self,
        messages: &[Message],
        system: Option<&str>,
        tools: &[ToolDefinition],
        config: &ProviderConfig,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, String>> + Send>>, String>;
}

pub fn collect_tool_calls(events: &[StreamEvent]) -> Vec<ToolCall> {
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for event in events {
        match event {
            StreamEvent::ToolUseStart { id, name } => {
                tool_calls.push(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: serde_json::Value::Null,
                });
            }
            StreamEvent::ToolUseInput { json } => {
                if let Some(tc) = tool_calls.last_mut() {
                    let existing = match &tc.input {
                        serde_json::Value::Null => String::new(),
                        serde_json::Value::String(s) => s.clone(),
                        _ => serde_json::to_string(&tc.input).unwrap_or_default(),
                    };
                    let combined = format!("{existing}{json}");
                    tc.input = serde_json::Value::String(combined);
                }
            }
            StreamEvent::Done { .. } => {
                for tc in &mut tool_calls {
                    if let serde_json::Value::String(s) = &tc.input {
                        if let Ok(parsed) = serde_json::from_str(s) {
                            tc.input = parsed;
                        }
                    }
                }
            }
            StreamEvent::ContentDelta { .. } => {}
        }
    }

    tool_calls
}

pub fn collect_text(events: &[StreamEvent]) -> Option<String> {
    let mut text = String::new();
    for event in events {
        if let StreamEvent::ContentDelta { text: t } = event {
            text.push_str(t);
        }
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod provider_helpers {
    use super::*;

    #[test]
    fn collect_text_from_deltas() {
        let events = vec![
            StreamEvent::ContentDelta {
                text: "Hello ".into(),
            },
            StreamEvent::ContentDelta {
                text: "world".into(),
            },
        ];
        assert_eq!(collect_text(&events), Some("Hello world".into()));
    }

    #[test]
    fn collect_text_empty_when_no_deltas() {
        let events = vec![StreamEvent::Done {
            stop_reason: "end_turn".into(),
        }];
        assert_eq!(collect_text(&events), None);
    }

    #[test]
    fn collect_tool_calls_assembles_from_events() {
        let events = vec![
            StreamEvent::ToolUseStart {
                id: "tc-1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolUseInput {
                json: r#"{"comm"#.into(),
            },
            StreamEvent::ToolUseInput {
                json: r#"and":"ls"}"#.into(),
            },
            StreamEvent::Done {
                stop_reason: "tool_use".into(),
            },
        ];
        let calls = collect_tool_calls(&events);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "tc-1");
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].input, serde_json::json!({"command": "ls"}));
    }

    #[test]
    fn collect_tool_calls_handles_multiple() {
        let events = vec![
            StreamEvent::ToolUseStart {
                id: "tc-1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolUseInput {
                json: r#"{"command":"ls"}"#.into(),
            },
            StreamEvent::ToolUseStart {
                id: "tc-2".into(),
                name: "read".into(),
            },
            StreamEvent::ToolUseInput {
                json: r#"{"path":"foo.rs"}"#.into(),
            },
            StreamEvent::Done {
                stop_reason: "tool_use".into(),
            },
        ];
        let calls = collect_tool_calls(&events);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[1].name, "read");
    }

    #[test]
    fn collect_tool_calls_keeps_raw_string_on_invalid_json() {
        let events = vec![
            StreamEvent::ToolUseStart {
                id: "tc-1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolUseInput {
                json: "not valid json{".into(),
            },
            StreamEvent::Done {
                stop_reason: "tool_use".into(),
            },
        ];
        let calls = collect_tool_calls(&events);
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].input.is_string(),
            "input should stay as raw string when JSON parsing fails"
        );
    }

    #[test]
    fn provider_enum_deserializes_from_lowercase() {
        let p: Provider = serde_json::from_str("\"anthropic\"").unwrap();
        assert_eq!(p, Provider::Anthropic);
    }

    #[test]
    fn provider_enum_rejects_unknown() {
        let result: Result<Provider, _> = serde_json::from_str("\"banana\"");
        assert!(result.is_err());
    }
}

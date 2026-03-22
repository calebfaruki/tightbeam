use crate::protocol::{Message, ToolDefinition};
use crate::provider::{LlmProvider, ProviderConfig, StreamEvent};
use crate::streaming;
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

pub struct ClaudeProvider {
    client: reqwest::Client,
}

impl Default for ClaudeProvider {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl ClaudeProvider {
    pub fn new() -> Self {
        Self::default()
    }
}

fn build_api_messages(messages: &[Message]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|m| {
            let mut obj = serde_json::Map::new();
            obj.insert("role".into(), serde_json::Value::String(m.role.clone()));

            if let Some(ref content) = m.content {
                obj.insert("content".into(), content.clone());
            }

            if let Some(ref tool_calls) = m.tool_calls {
                let content_blocks: Vec<serde_json::Value> = tool_calls
                    .iter()
                    .map(|tc| {
                        serde_json::json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.name,
                            "input": tc.input,
                        })
                    })
                    .collect();

                if let Some(serde_json::Value::String(text)) = m.content.as_ref() {
                    let mut blocks = vec![serde_json::json!({
                        "type": "text",
                        "text": text,
                    })];
                    blocks.extend(content_blocks);
                    obj.insert("content".into(), serde_json::Value::Array(blocks));
                } else {
                    obj.insert("content".into(), serde_json::Value::Array(content_blocks));
                }
            }

            if m.role == "tool" {
                if let Some(ref tool_call_id) = m.tool_call_id {
                    obj.remove("role");
                    obj.insert("role".into(), serde_json::Value::String("user".into()));

                    let content_text = m
                        .content
                        .as_ref()
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();

                    obj.insert(
                        "content".into(),
                        serde_json::json!([{
                            "type": "tool_result",
                            "tool_use_id": tool_call_id,
                            "content": content_text,
                        }]),
                    );
                }
            }

            serde_json::Value::Object(obj)
        })
        .collect()
}

fn build_api_tools(tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
            })
        })
        .collect()
}

#[async_trait]
impl LlmProvider for ClaudeProvider {
    async fn call(
        &self,
        messages: &[Message],
        system: Option<&str>,
        tools: &[ToolDefinition],
        config: &ProviderConfig,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, String>> + Send>>, String> {
        let mut body = serde_json::Map::new();
        body.insert(
            "model".into(),
            serde_json::Value::String(config.model.clone()),
        );
        body.insert(
            "max_tokens".into(),
            serde_json::Value::Number(config.max_tokens.into()),
        );
        body.insert("stream".into(), serde_json::Value::Bool(true));

        if let Some(sys) = system {
            body.insert("system".into(), serde_json::Value::String(sys.to_string()));
        }

        body.insert(
            "messages".into(),
            serde_json::Value::Array(build_api_messages(messages)),
        );

        let api_tools = build_api_tools(tools);
        if !api_tools.is_empty() {
            body.insert("tools".into(), serde_json::Value::Array(api_tools));
        }

        let response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &config.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {e}"))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("API error {status}: {body}"));
        }

        let stream = streaming::parse_sse_stream(response);
        Ok(stream)
    }
}

#[cfg(test)]
mod claude_api {
    use super::*;
    use crate::protocol::ToolCall;

    #[test]
    fn user_message_converts_to_api_format() {
        let messages = vec![Message {
            role: "user".into(),
            content: Some(serde_json::Value::String("Hello".into())),
            tool_calls: None,
            tool_call_id: None,
        }];
        let api = build_api_messages(&messages);
        assert_eq!(api.len(), 1);
        assert_eq!(api[0]["role"], "user");
        assert_eq!(api[0]["content"], "Hello");
    }

    #[test]
    fn assistant_with_tool_calls_converts() {
        let messages = vec![Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "tc-1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            }]),
            tool_call_id: None,
        }];
        let api = build_api_messages(&messages);
        let content = api[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "tool_use");
        assert_eq!(content[0]["id"], "tc-1");
        assert_eq!(content[0]["name"], "bash");
    }

    #[test]
    fn tool_result_converts_to_user_with_tool_result_block() {
        let messages = vec![Message {
            role: "tool".into(),
            content: Some(serde_json::Value::String("file list here".into())),
            tool_calls: None,
            tool_call_id: Some("tc-1".into()),
        }];
        let api = build_api_messages(&messages);
        assert_eq!(api[0]["role"], "user");
        let content = api[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "tc-1");
        assert_eq!(content[0]["content"], "file list here");
    }

    #[test]
    fn tools_convert_to_api_format() {
        let tools = vec![ToolDefinition {
            name: "bash".into(),
            description: "Run a shell command".into(),
            input_schema: serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        }];
        let api = build_api_tools(&tools);
        assert_eq!(api.len(), 1);
        assert_eq!(api[0]["name"], "bash");
        assert_eq!(api[0]["description"], "Run a shell command");
    }
}

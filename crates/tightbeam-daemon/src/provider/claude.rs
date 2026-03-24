use crate::protocol::{ContentBlock, Message, ToolDefinition};
use crate::provider::{LlmProvider, ProviderConfig, StreamEvent};
use crate::streaming;
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;
use tightbeam_protocol::content_text;

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

fn content_block_to_api(block: &ContentBlock) -> serde_json::Value {
    match block {
        ContentBlock::Text { text } => serde_json::json!({
            "type": "text",
            "text": text,
        }),
        ContentBlock::Image { media_type, data } => serde_json::json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data,
            }
        }),
        ContentBlock::FileIncoming { .. } => {
            panic!("FileIncoming must be replaced before reaching provider")
        }
    }
}

fn build_api_messages(messages: &[Message]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|m| {
            let mut obj = serde_json::Map::new();

            if m.role == "tool" {
                obj.insert("role".into(), "user".into());
                if let Some(ref tool_call_id) = m.tool_call_id {
                    let text = content_text(&m.content).unwrap_or("").to_string();
                    obj.insert(
                        "content".into(),
                        serde_json::json!([{
                            "type": "tool_result",
                            "tool_use_id": tool_call_id,
                            "content": text,
                        }]),
                    );
                }
            } else if let Some(ref tool_calls) = m.tool_calls {
                obj.insert("role".into(), m.role.clone().into());
                let mut content_blocks: Vec<serde_json::Value> = m
                    .content
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(content_block_to_api)
                    .collect();
                for tc in tool_calls {
                    content_blocks.push(serde_json::json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": tc.input,
                    }));
                }
                obj.insert("content".into(), serde_json::Value::Array(content_blocks));
            } else {
                obj.insert("role".into(), m.role.clone().into());
                if let Some(ref blocks) = m.content {
                    let api_blocks: Vec<serde_json::Value> =
                        blocks.iter().map(content_block_to_api).collect();
                    obj.insert("content".into(), serde_json::Value::Array(api_blocks));
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
                "input_schema": t.parameters,
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
            content: Some(ContentBlock::text_content("Hello")),
            tool_calls: None,
            tool_call_id: None,
            is_error: None,
        }];
        let api = build_api_messages(&messages);
        assert_eq!(api.len(), 1);
        assert_eq!(api[0]["role"], "user");
        assert_eq!(api[0]["content"][0]["type"], "text");
        assert_eq!(api[0]["content"][0]["text"], "Hello");
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
            is_error: None,
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
            content: Some(ContentBlock::text_content("file list here")),
            tool_calls: None,
            tool_call_id: Some("tc-1".into()),
            is_error: None,
        }];
        let api = build_api_messages(&messages);
        assert_eq!(api[0]["role"], "user");
        let content = api[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "tc-1");
        assert_eq!(content[0]["content"], "file list here");
    }

    #[test]
    fn image_block_converts_to_anthropic_format() {
        let messages = vec![Message {
            role: "user".into(),
            content: Some(vec![
                ContentBlock::text("Describe this"),
                ContentBlock::image("image/png", "iVBOR..."),
            ]),
            tool_calls: None,
            tool_call_id: None,
            is_error: None,
        }];
        let api = build_api_messages(&messages);
        let content = api[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "Describe this");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "iVBOR...");
    }

    #[test]
    #[should_panic(expected = "FileIncoming must be replaced")]
    fn file_incoming_panics_in_provider() {
        let messages = vec![Message {
            role: "user".into(),
            content: Some(vec![ContentBlock::file_incoming("f.png", "image/png", 1)]),
            tool_calls: None,
            tool_call_id: None,
            is_error: None,
        }];
        build_api_messages(&messages);
    }

    #[test]
    fn tools_convert_to_api_format() {
        let tools = vec![ToolDefinition {
            name: "bash".into(),
            description: "Run a shell command".into(),
            parameters: serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        }];
        let api = build_api_tools(&tools);
        assert_eq!(api.len(), 1);
        assert_eq!(api[0]["name"], "bash");
        assert_eq!(api[0]["description"], "Run a shell command");
    }
}

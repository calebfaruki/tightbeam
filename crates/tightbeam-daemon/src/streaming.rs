use crate::provider::StreamEvent;
use futures::stream::{self, Stream};
use std::pin::Pin;

pub fn parse_sse_stream(
    response: reqwest::Response,
) -> Pin<Box<dyn Stream<Item = Result<StreamEvent, String>> + Send>> {
    let byte_stream = response.bytes_stream();

    let event_stream = stream::unfold(
        (byte_stream, String::new()),
        |(mut byte_stream, mut buffer)| async move {
            use futures::TryStreamExt;

            loop {
                if let Some(pos) = buffer.find("\n\n") {
                    let event_text = buffer[..pos].to_string();
                    buffer = buffer[pos + 2..].to_string();

                    if let Some(event) = parse_sse_event(&event_text) {
                        return Some((Ok(event), (byte_stream, buffer)));
                    }
                    continue;
                }

                match byte_stream.try_next().await {
                    Ok(Some(chunk)) => {
                        buffer.push_str(&String::from_utf8_lossy(&chunk));
                    }
                    Ok(None) => {
                        if !buffer.trim().is_empty() {
                            if let Some(event) = parse_sse_event(&buffer) {
                                buffer.clear();
                                return Some((Ok(event), (byte_stream, buffer)));
                            }
                        }
                        return None;
                    }
                    Err(e) => {
                        return Some((Err(format!("stream error: {e}")), (byte_stream, buffer)));
                    }
                }
            }
        },
    );

    Box::pin(event_stream)
}

fn parse_sse_event(text: &str) -> Option<StreamEvent> {
    let mut event_type = None;
    let mut data_lines = Vec::new();

    for line in text.lines() {
        if let Some(stripped) = line.strip_prefix("event: ") {
            event_type = Some(stripped.trim().to_string());
        } else if let Some(stripped) = line.strip_prefix("data: ") {
            data_lines.push(stripped.to_string());
        }
    }

    let event_type = event_type?;
    let data = data_lines.join("\n");

    match event_type.as_str() {
        "content_block_delta" => {
            let parsed: serde_json::Value = serde_json::from_str(&data).ok()?;
            let delta = parsed.get("delta")?;
            let delta_type = delta.get("type")?.as_str()?;

            match delta_type {
                "text_delta" => {
                    let text = delta.get("text")?.as_str()?.to_string();
                    Some(StreamEvent::ContentDelta { text })
                }
                "input_json_delta" => {
                    let json = delta.get("partial_json")?.as_str()?.to_string();
                    Some(StreamEvent::ToolUseInput { json })
                }
                _ => None,
            }
        }
        "content_block_start" => {
            let parsed: serde_json::Value = serde_json::from_str(&data).ok()?;
            let block = parsed.get("content_block")?;
            let block_type = block.get("type")?.as_str()?;

            if block_type == "tool_use" {
                let id = block.get("id")?.as_str()?.to_string();
                let name = block.get("name")?.as_str()?.to_string();
                Some(StreamEvent::ToolUseStart { id, name })
            } else {
                None
            }
        }
        "message_delta" => {
            let parsed: serde_json::Value = serde_json::from_str(&data).ok()?;
            let delta = parsed.get("delta")?;
            let stop_reason = delta.get("stop_reason")?.as_str()?.to_string();
            Some(StreamEvent::Done { stop_reason })
        }
        "message_stop" | "message_start" | "content_block_stop" | "ping" => None,
        _ => None,
    }
}

#[cfg(test)]
mod sse_parsing {
    use super::*;

    #[test]
    fn text_delta_parses() {
        let text = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}";
        let event = parse_sse_event(text).unwrap();
        match event {
            StreamEvent::ContentDelta { text } => assert_eq!(text, "Hello"),
            _ => panic!("expected ContentDelta"),
        }
    }

    #[test]
    fn tool_use_start_parses() {
        let text = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tc-001\",\"name\":\"bash\",\"input\":{}}}";
        let event = parse_sse_event(text).unwrap();
        match event {
            StreamEvent::ToolUseStart { id, name } => {
                assert_eq!(id, "tc-001");
                assert_eq!(name, "bash");
            }
            _ => panic!("expected ToolUseStart"),
        }
    }

    #[test]
    fn input_json_delta_parses() {
        let text = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\"\"}}";
        let event = parse_sse_event(text).unwrap();
        match event {
            StreamEvent::ToolUseInput { json } => assert_eq!(json, "{\"command\""),
            _ => panic!("expected ToolUseInput"),
        }
    }

    #[test]
    fn message_delta_with_stop_reason_parses() {
        let text = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}";
        let event = parse_sse_event(text).unwrap();
        match event {
            StreamEvent::Done { stop_reason } => assert_eq!(stop_reason, "end_turn"),
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn message_stop_returns_none() {
        let text = "event: message_stop\ndata: {\"type\":\"message_stop\"}";
        assert!(parse_sse_event(text).is_none());
    }

    #[test]
    fn ping_returns_none() {
        let text = "event: ping\ndata: {}";
        assert!(parse_sse_event(text).is_none());
    }

    #[test]
    fn text_block_start_returns_none() {
        let text = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}";
        assert!(parse_sse_event(text).is_none());
    }

    #[test]
    fn tool_use_stop_reason_parses() {
        let text = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}";
        let event = parse_sse_event(text).unwrap();
        match event {
            StreamEvent::Done { stop_reason } => assert_eq!(stop_reason, "tool_use"),
            _ => panic!("expected Done"),
        }
    }
}

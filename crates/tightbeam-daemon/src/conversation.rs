use crate::protocol::{Message, ToolDefinition};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
struct LogEntry {
    ts: String,
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<crate::protocol::ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
}

pub struct ConversationLog {
    messages: Vec<Message>,
    system_prompt: Option<String>,
    tools: Vec<ToolDefinition>,
    log_path: PathBuf,
}

impl ConversationLog {
    pub fn new(log_dir: &Path) -> Self {
        let log_path = log_dir.join("conversation.ndjson");
        Self {
            messages: Vec::new(),
            system_prompt: None,
            tools: Vec::new(),
            log_path,
        }
    }

    pub fn rebuild(log_dir: &Path) -> Result<Self, String> {
        let log_path = log_dir.join("conversation.ndjson");
        let mut messages = Vec::new();

        if log_path.exists() {
            let file = fs::File::open(&log_path).map_err(|e| format!("failed to open log: {e}"))?;
            let reader = BufReader::new(file);

            for line in reader.lines() {
                let line = line.map_err(|e| format!("failed to read log line: {e}"))?;
                if line.is_empty() {
                    continue;
                }
                let entry: LogEntry = serde_json::from_str(&line)
                    .map_err(|e| format!("failed to parse log entry: {e}"))?;
                messages.push(Message {
                    role: entry.role,
                    content: entry.content,
                    tool_calls: entry.tool_calls,
                    tool_call_id: entry.tool_call_id,
                    is_error: entry.is_error,
                });
            }
        }

        Ok(Self {
            messages,
            system_prompt: None,
            tools: Vec::new(),
            log_path,
        })
    }

    pub fn set_system_prompt(&mut self, prompt: String) {
        if self.system_prompt.is_none() {
            self.system_prompt = Some(prompt);
        }
    }

    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    pub fn set_tools(&mut self, tools: Vec<ToolDefinition>) {
        self.tools = tools;
    }

    pub fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }

    pub fn append(&mut self, message: Message) -> Result<(), String> {
        self.write_to_log(&message)?;
        self.messages.push(message);
        Ok(())
    }

    pub fn append_many(&mut self, messages: Vec<Message>) -> Result<(), String> {
        for message in messages {
            self.append(message)?;
        }
        Ok(())
    }

    pub fn history(&self) -> &[Message] {
        &self.messages
    }

    fn write_to_log(&self, message: &Message) -> Result<(), String> {
        if let Some(parent) = self.log_path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("failed to create log dir: {e}"))?;
        }

        let entry = LogEntry {
            ts: Utc::now().to_rfc3339(),
            role: message.role.clone(),
            content: message.content.clone(),
            tool_calls: message.tool_calls.clone(),
            tool_call_id: message.tool_call_id.clone(),
            is_error: message.is_error,
        };

        let mut line = serde_json::to_string(&entry)
            .map_err(|e| format!("failed to serialize log entry: {e}"))?;
        line.push('\n');

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .map_err(|e| format!("failed to open log file: {e}"))?;

        file.write_all(line.as_bytes())
            .map_err(|e| format!("failed to write log entry: {e}"))?;

        Ok(())
    }
}

#[cfg(test)]
mod conversation_accumulation {
    use super::*;
    use tempfile::TempDir;

    fn text_msg(role: &str, text: &str) -> Message {
        Message {
            role: role.into(),
            content: Some(serde_json::Value::String(text.into())),
            tool_calls: None,
            tool_call_id: None,
            is_error: None,
        }
    }

    #[test]
    fn new_log_starts_empty() {
        let tmp = TempDir::new().unwrap();
        let log = ConversationLog::new(tmp.path());
        assert!(log.history().is_empty());
        assert!(log.system_prompt().is_none());
    }

    #[test]
    fn append_adds_to_history_and_log_file() {
        let tmp = TempDir::new().unwrap();
        let mut log = ConversationLog::new(tmp.path());

        log.append(text_msg("user","Hello")).unwrap();
        log.append(text_msg("assistant","Hi there")).unwrap();

        assert_eq!(log.history().len(), 2);
        assert_eq!(log.history()[0].role, "user");
        assert_eq!(log.history()[1].role, "assistant");

        let log_file = tmp.path().join("conversation.ndjson");
        assert!(log_file.exists());
        let content = fs::read_to_string(&log_file).unwrap();
        let lines: Vec<&str> = content.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn rebuild_restores_history_from_log() {
        let tmp = TempDir::new().unwrap();

        {
            let mut log = ConversationLog::new(tmp.path());
            log.append(text_msg("user","First")).unwrap();
            log.append(text_msg("assistant","Second")).unwrap();
            log.append(text_msg("user","Third")).unwrap();
        }

        let rebuilt = ConversationLog::rebuild(tmp.path()).unwrap();
        assert_eq!(rebuilt.history().len(), 3);
        assert_eq!(rebuilt.history()[0].role, "user");
        assert_eq!(rebuilt.history()[1].role, "assistant");
        assert_eq!(rebuilt.history()[2].role, "user");
    }

    #[test]
    fn rebuild_empty_dir_returns_empty_log() {
        let tmp = TempDir::new().unwrap();
        let rebuilt = ConversationLog::rebuild(tmp.path()).unwrap();
        assert!(rebuilt.history().is_empty());
    }

    #[test]
    fn system_prompt_cached_on_first_set() {
        let tmp = TempDir::new().unwrap();
        let mut log = ConversationLog::new(tmp.path());

        log.set_system_prompt("You are helpful.".into());
        assert_eq!(log.system_prompt(), Some("You are helpful."));

        log.set_system_prompt("Ignored.".into());
        assert_eq!(log.system_prompt(), Some("You are helpful."));
    }

    #[test]
    fn tool_result_message_round_trips() {
        let tmp = TempDir::new().unwrap();
        let mut log = ConversationLog::new(tmp.path());

        let msg = Message {
            role: "tool".into(),
            content: Some(serde_json::Value::String("ls output".into())),
            tool_calls: None,
            tool_call_id: Some("tc-001".into()),
            is_error: None,
        };
        log.append(msg).unwrap();

        let rebuilt = ConversationLog::rebuild(tmp.path()).unwrap();
        assert_eq!(rebuilt.history().len(), 1);
        assert_eq!(rebuilt.history()[0].role, "tool");
        assert_eq!(rebuilt.history()[0].tool_call_id.as_deref(), Some("tc-001"));
    }

    #[test]
    fn tool_definitions_cached_across_calls() {
        let tmp = TempDir::new().unwrap();
        let mut log = ConversationLog::new(tmp.path());

        assert!(log.tools().is_empty());

        let tools = vec![ToolDefinition {
            name: "bash".into(),
            description: "Run a command".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        log.set_tools(tools);
        assert_eq!(log.tools().len(), 1);
        assert_eq!(log.tools()[0].name, "bash");

        // Updating tools replaces them
        let new_tools = vec![
            ToolDefinition {
                name: "bash".into(),
                description: "Run a command".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
            ToolDefinition {
                name: "read".into(),
                description: "Read a file".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
        ];
        log.set_tools(new_tools);
        assert_eq!(log.tools().len(), 2);
    }

    #[test]
    fn assistant_with_tool_calls_round_trips() {
        let tmp = TempDir::new().unwrap();
        let mut log = ConversationLog::new(tmp.path());

        let msg = Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![crate::protocol::ToolCall {
                id: "tc-001".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            }]),
            tool_call_id: None,
            is_error: None,
        };
        log.append(msg).unwrap();

        let rebuilt = ConversationLog::rebuild(tmp.path()).unwrap();
        let tool_calls = rebuilt.history()[0].tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "bash");
    }

    #[test]
    fn rebuild_fails_on_corrupted_log() {
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("conversation.ndjson");
        std::fs::write(
            &log_path,
            "{\"ts\":\"t\",\"role\":\"user\",\"content\":\"ok\"}\nnot json\n",
        )
        .unwrap();
        assert!(
            ConversationLog::rebuild(tmp.path()).is_err(),
            "should fail on corrupted log entry"
        );
    }
}

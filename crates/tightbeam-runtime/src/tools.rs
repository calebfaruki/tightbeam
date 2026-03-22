use tightbeam_protocol::ToolDefinition;

pub(crate) fn tool_definitions(enabled: &[String]) -> Vec<ToolDefinition> {
    let all = [
        ToolDefinition {
            name: "bash".into(),
            description: "Run a shell command and return stdout, stderr, and exit code".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    }
                },
                "required": ["command"]
            }),
        },
        ToolDefinition {
            name: "read_file".into(),
            description: "Read the contents of a file".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to read"
                    }
                },
                "required": ["path"]
            }),
        },
        ToolDefinition {
            name: "write_file".into(),
            description: "Write content to a file, creating it if it doesn't exist".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to write to"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write"
                    }
                },
                "required": ["path", "content"]
            }),
        },
        ToolDefinition {
            name: "list_directory".into(),
            description: "List the contents of a directory".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the directory to list"
                    }
                },
                "required": ["path"]
            }),
        },
    ];

    all.into_iter()
        .filter(|t| enabled.iter().any(|e| e == &t.name))
        .collect()
}

pub(crate) async fn execute_tool(
    name: &str,
    input: &serde_json::Value,
    max_output_chars: usize,
) -> (String, bool) {
    let (output, is_error) = match name {
        "bash" => execute_bash(input).await,
        "read_file" => execute_read_file(input).await,
        "write_file" => execute_write_file(input).await,
        "list_directory" => execute_list_directory(input).await,
        _ => (format!("unknown tool: {name}"), true),
    };

    (truncate_middle(&output, max_output_chars), is_error)
}

fn require_str<'a>(input: &'a serde_json::Value, key: &str) -> Result<&'a str, (String, bool)> {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| (format!("missing required parameter: {key}"), true))
}

async fn execute_bash(input: &serde_json::Value) -> (String, bool) {
    let command = match require_str(input, "command") {
        Ok(c) => c,
        Err(e) => return e,
    };

    // sh -c is intentional: the runtime runs in a sandboxed container with no
    // credentials. The LLM needs shell features (pipes, redirects, etc.).
    // The container boundary is the security boundary.
    match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .await
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let code = output.status.code().unwrap_or(-1);
            let is_error = !output.status.success();

            let result = if stderr.is_empty() {
                format!("{stdout}exit code: {code}")
            } else {
                format!("{stdout}stderr:\n{stderr}exit code: {code}")
            };
            (result, is_error)
        }
        Err(e) => (format!("failed to execute command: {e}"), true),
    }
}

async fn execute_read_file(input: &serde_json::Value) -> (String, bool) {
    let path = match require_str(input, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };

    match tokio::fs::read_to_string(path).await {
        Ok(content) => (content, false),
        Err(e) => (format!("failed to read {path}: {e}"), true),
    }
}

async fn execute_write_file(input: &serde_json::Value) -> (String, bool) {
    let path = match require_str(input, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let content = match require_str(input, "content") {
        Ok(c) => c,
        Err(e) => return e,
    };

    match tokio::fs::write(path, content).await {
        Ok(()) => (format!("wrote {} bytes to {path}", content.len()), false),
        Err(e) => (format!("failed to write {path}: {e}"), true),
    }
}

async fn execute_list_directory(input: &serde_json::Value) -> (String, bool) {
    let path = match require_str(input, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };

    match tokio::fs::read_dir(path).await {
        Ok(mut entries) => {
            let mut names = Vec::new();
            while let Ok(Some(entry)) = entries.next_entry().await {
                names.push(entry.file_name().to_string_lossy().to_string());
            }
            names.sort();
            (names.join("\n"), false)
        }
        Err(e) => (format!("failed to list {path}: {e}"), true),
    }
}

pub(crate) fn truncate_middle(output: &str, max_chars: usize) -> String {
    if output.len() <= max_chars {
        return output.to_string();
    }

    let marker_template = "\n[...truncated 0 characters...]\n";
    let digit_count = (output.len() - max_chars).to_string().len();
    let marker_len = marker_template.len() - 1 + digit_count; // -1 for the "0", +digit_count

    if max_chars <= marker_len {
        return output[..max_chars].to_string();
    }

    let available = max_chars - marker_len;
    let head_len = available * 2 / 5;
    let tail_len = available - head_len;

    let head = &output[..head_len];
    let tail = &output[output.len() - tail_len..];
    let truncated = output.len() - head_len - tail_len;

    format!("{head}\n[...truncated {truncated} characters...]\n{tail}")
}

#[cfg(test)]
mod tool_tests {
    use super::*;

    #[test]
    fn truncate_short_string_unchanged() {
        let s = "hello world";
        assert_eq!(truncate_middle(s, 100), s);
    }

    #[test]
    fn truncate_exact_limit_unchanged() {
        let s = "hello";
        assert_eq!(truncate_middle(s, 5), s);
    }

    #[test]
    fn truncate_long_string_has_marker() {
        let s = "a".repeat(1000);
        let result = truncate_middle(&s, 100);
        assert_eq!(result.len(), 100, "should produce exactly max_chars");
        // Verify marker is well-formed: extract the count and check it's a valid number
        let marker_start = result.find("[...truncated ").expect("marker missing");
        let after_prefix = &result[marker_start + "[...truncated ".len()..];
        let count_end = after_prefix.find(' ').expect("marker malformed");
        let count: usize = after_prefix[..count_end]
            .parse()
            .expect("truncated count should be a number");
        assert!(count > 0, "truncated count should be positive");
        assert!(result.contains(&format!("[...truncated {count} characters...]")));
    }

    #[test]
    fn truncate_preserves_head_and_tail_with_ratio() {
        // Build a string where we can verify the ~40% head / ~60% tail split
        let head_region = "H".repeat(50);
        let tail_region = "T".repeat(50);
        let s = format!("{}{}{}", head_region, "x".repeat(900), tail_region);
        let result = truncate_middle(&s, 100);
        assert!(result.starts_with("HH"), "should start with head region");
        assert!(result.ends_with("TT"), "should end with tail region");
        // With 100 max chars and ~30 char marker, available ≈ 70 chars
        // Head gets 2/5 ≈ 28 chars, tail gets 3/5 ≈ 42 chars
        let marker_pos = result.find("[...truncated").unwrap();
        let head_len = marker_pos;
        // Head should be roughly 40% of available space (not 50% or 10%)
        assert!(
            (15..=40).contains(&head_len),
            "head should be ~40% of available, got {head_len}"
        );
    }

    #[test]
    fn tool_definitions_filters_by_enabled() {
        let enabled: Vec<String> = vec!["bash".into(), "read_file".into()];
        let defs = tool_definitions(&enabled);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["bash", "read_file"]);
    }

    #[test]
    fn tool_definitions_excludes_non_enabled() {
        let enabled: Vec<String> = vec!["bash".into()];
        let defs = tool_definitions(&enabled);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "bash");
        assert!(defs.iter().all(|d| d.name != "read_file"));
        assert!(defs.iter().all(|d| d.name != "write_file"));
    }

    #[test]
    fn tool_definitions_empty_for_no_match() {
        let enabled: Vec<String> = vec!["nonexistent".into()];
        let defs = tool_definitions(&enabled);
        assert!(defs.is_empty());
    }

    #[test]
    fn tool_definitions_all_four() {
        let enabled: Vec<String> = vec![
            "bash".into(),
            "read_file".into(),
            "write_file".into(),
            "list_directory".into(),
        ];
        let defs = tool_definitions(&enabled);
        assert_eq!(defs.len(), 4);
    }

    #[tokio::test]
    async fn bash_echo_succeeds() {
        let input = serde_json::json!({"command": "echo hello"});
        let (output, is_error) = execute_tool("bash", &input, 30000).await;
        assert!(!is_error);
        assert!(output.contains("hello"));
        assert!(output.contains("exit code: 0"));
    }

    #[tokio::test]
    async fn bash_failing_command() {
        let input = serde_json::json!({"command": "exit 42"});
        let (output, is_error) = execute_tool("bash", &input, 30000).await;
        assert!(is_error);
        assert!(output.contains("exit code: 42"));
    }

    #[tokio::test]
    async fn bash_missing_command_param() {
        let input = serde_json::json!({});
        let (output, is_error) = execute_tool("bash", &input, 30000).await;
        assert!(is_error);
        assert!(output.contains("missing required parameter"));
    }

    #[tokio::test]
    async fn read_file_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "file content").unwrap();

        let input = serde_json::json!({"path": path.to_str().unwrap()});
        let (output, is_error) = execute_tool("read_file", &input, 30000).await;
        assert!(!is_error);
        assert_eq!(output, "file content");
    }

    #[tokio::test]
    async fn read_file_missing_file() {
        let input = serde_json::json!({"path": "/nonexistent/file.txt"});
        let (output, is_error) = execute_tool("read_file", &input, 30000).await;
        assert!(is_error);
        assert!(output.contains("failed to read"));
    }

    #[tokio::test]
    async fn write_file_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");

        let input = serde_json::json!({"path": path.to_str().unwrap(), "content": "hello"});
        let (output, is_error) = execute_tool("write_file", &input, 30000).await;
        assert!(!is_error);
        assert!(output.contains("wrote 5 bytes"));

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "hello");
    }

    #[tokio::test]
    async fn list_directory_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();

        let input = serde_json::json!({"path": dir.path().to_str().unwrap()});
        let (output, is_error) = execute_tool("list_directory", &input, 30000).await;
        assert!(!is_error);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines, vec!["a.txt", "b.txt"]);
    }

    #[tokio::test]
    async fn list_directory_missing_dir() {
        let input = serde_json::json!({"path": "/nonexistent/dir"});
        let (output, is_error) = execute_tool("list_directory", &input, 30000).await;
        assert!(is_error);
        assert!(output.contains("failed to list"));
    }

    #[tokio::test]
    async fn unknown_tool_returns_error() {
        let input = serde_json::json!({});
        let (output, is_error) = execute_tool("bogus", &input, 30000).await;
        assert!(is_error);
        assert!(output.contains("unknown tool"));
    }

    #[tokio::test]
    async fn tool_output_truncated_at_exact_limit() {
        let input = serde_json::json!({"command": "yes | head -n 10000"});
        let (output, is_error) = execute_tool("bash", &input, 200).await;
        assert!(!is_error);
        assert_eq!(
            output.len(),
            200,
            "truncation should produce exactly max_chars output"
        );
        assert!(output.contains("[...truncated"));
    }

    #[tokio::test]
    async fn execute_tool_propagates_error_flag_on_failure() {
        let input = serde_json::json!({"command": "exit 1"});
        let (_, is_error) = execute_tool("bash", &input, 30000).await;
        assert!(
            is_error,
            "non-zero exit should propagate is_error=true through execute_tool"
        );
    }

    #[tokio::test]
    async fn execute_tool_propagates_success_flag() {
        let input = serde_json::json!({"command": "echo ok"});
        let (_, is_error) = execute_tool("bash", &input, 30000).await;
        assert!(
            !is_error,
            "zero exit should propagate is_error=false through execute_tool"
        );
    }
}

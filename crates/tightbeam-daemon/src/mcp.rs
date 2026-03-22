use crate::profile::ResolvedMcp;
use crate::protocol::{Message, ToolCall, ToolDefinition};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) fn tool_result_message(
    tool_call_id: String,
    content: String,
    is_error: bool,
) -> Message {
    Message {
        role: "tool".into(),
        content: Some(serde_json::Value::String(content)),
        tool_calls: None,
        tool_call_id: Some(tool_call_id),
        is_error: if is_error { Some(true) } else { None },
    }
}

// --- MCP Connection (single remote server) ---

pub struct McpConnection {
    url: String,
    auth_token: String,
    client: reqwest::Client,
    tools: Vec<ToolDefinition>,
    next_id: AtomicU64,
    connected: bool,
}

impl McpConnection {
    pub fn new(url: String, auth_token: String) -> Self {
        Self {
            url,
            auth_token,
            client: reqwest::Client::new(),
            tools: Vec::new(),
            next_id: AtomicU64::new(1),
            connected: false,
        }
    }

    fn next_request_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub async fn discover_tools(
        &mut self,
        allowlist: Option<&[String]>,
    ) -> Result<Vec<ToolDefinition>, String> {
        let id = self.next_request_id();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/list"
        });

        let response = self
            .client
            .post(&self.url)
            .header("Authorization", format!("Bearer {}", self.auth_token))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("MCP connect error: {e}"))?;

        let rpc_result = parse_mcp_response(response)
            .await
            .map_err(|e| format!("MCP tools/list error: {e}"))?;

        let tools_array = rpc_result
            .get("tools")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "MCP tools/list: missing tools array".to_string())?;

        let mut tools = Vec::new();
        for tool_val in tools_array {
            let name = tool_val
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if let Some(allow) = allowlist {
                if allow.is_empty() || !allow.contains(&name) {
                    continue;
                }
            }

            let description = tool_val
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let parameters = tool_val
                .get("inputSchema")
                .cloned()
                .unwrap_or(serde_json::json!({"type": "object"}));

            tools.push(ToolDefinition {
                name,
                description,
                parameters,
            });
        }

        self.tools = tools.clone();
        self.connected = true;
        Ok(tools)
    }

    async fn try_call_tool(&self, tool_call: &ToolCall) -> Result<String, String> {
        let id = self.next_request_id();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": tool_call.name,
                "arguments": tool_call.input
            }
        });

        let response = self
            .client
            .post(&self.url)
            .header("Authorization", format!("Bearer {}", self.auth_token))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("MCP call error: {e}"))?;

        let rpc_result = parse_mcp_response(response).await?;
        Ok(extract_content_text(&rpc_result))
    }

    pub async fn call_tool(&mut self, tool_call: &ToolCall) -> Message {
        let result = self.try_call_tool(tool_call).await;

        // On connection error, attempt one reconnect
        let result = match &result {
            Err(e) if e.contains("MCP call error") && self.connected => {
                self.connected = false;
                tracing::warn!("MCP connection lost, attempting reconnect for {}", self.url);
                self.try_call_tool(tool_call).await
            }
            _ => result,
        };

        match result {
            Ok(text) => {
                self.connected = true;
                tool_result_message(tool_call.id.clone(), text, false)
            }
            Err(e) => {
                self.connected = false;
                tool_result_message(tool_call.id.clone(), e, true)
            }
        }
    }
}

fn extract_content_text(result: &serde_json::Value) -> String {
    if let Some(content) = result.get("content").and_then(|v| v.as_array()) {
        let mut text = String::new();
        for block in content {
            if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
        }
        text
    } else if let Some(text) = result.get("content").and_then(|v| v.as_str()) {
        text.to_string()
    } else {
        serde_json::to_string(result).unwrap_or_default()
    }
}

async fn parse_mcp_response(response: reqwest::Response) -> Result<serde_json::Value, String> {
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let body = response
        .text()
        .await
        .map_err(|e| format!("failed to read MCP response: {e}"))?;

    if content_type.contains("text/event-stream") {
        parse_sse_jsonrpc(&body)
    } else {
        parse_json_jsonrpc(&body)
    }
}

fn parse_json_jsonrpc(body: &str) -> Result<serde_json::Value, String> {
    let rpc: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("invalid JSON-RPC response: {e}"))?;

    if let Some(err) = rpc.get("error") {
        let msg = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(format!("MCP error: {msg}"));
    }

    rpc.get("result")
        .cloned()
        .ok_or_else(|| "MCP response missing result".into())
}

fn parse_sse_jsonrpc(body: &str) -> Result<serde_json::Value, String> {
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            if let Ok(rpc) = serde_json::from_str::<serde_json::Value>(data) {
                if rpc.get("result").is_some() || rpc.get("error").is_some() {
                    return parse_json_jsonrpc(data);
                }
            }
        }
    }
    Err("no JSON-RPC result found in SSE stream".into())
}

// --- MCP Manager (coordinates multiple servers) ---

struct McpServerState {
    name: String,
    connection: McpConnection,
    allowlist: Option<Vec<String>>,
}

pub struct McpManager {
    servers: Vec<McpServerState>,
    tool_index: HashMap<String, usize>,
    all_tools: Vec<ToolDefinition>,
    initialized: bool,
}

impl McpManager {
    pub fn new(configs: Vec<ResolvedMcp>) -> Self {
        let servers = configs
            .into_iter()
            .map(|c| {
                let auth_token = std::env::var(&c.auth_env).unwrap_or_default();
                McpServerState {
                    name: c.name,
                    connection: McpConnection::new(c.url, auth_token),
                    allowlist: c.tool_allowlist,
                }
            })
            .collect();

        Self {
            servers,
            tool_index: HashMap::new(),
            all_tools: Vec::new(),
            initialized: false,
        }
    }

    pub async fn initialize(&mut self) -> Result<(), String> {
        self.tool_index.clear();
        self.all_tools.clear();

        for (idx, server) in self.servers.iter_mut().enumerate() {
            let tools = server
                .connection
                .discover_tools(server.allowlist.as_deref())
                .await
                .map_err(|e| format!("MCP server '{}': {e}", server.name))?;

            for tool in &tools {
                self.tool_index.insert(tool.name.clone(), idx);
            }
            self.all_tools.extend(tools);
        }

        self.initialized = true;
        Ok(())
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn has_servers(&self) -> bool {
        !self.servers.is_empty()
    }

    pub fn mcp_tools(&self) -> &[ToolDefinition] {
        &self.all_tools
    }

    pub fn is_mcp_tool(&self, name: &str) -> bool {
        self.tool_index.contains_key(name)
    }

    pub async fn execute_tool_calls(&mut self, calls: &[ToolCall]) -> Vec<Message> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let msg = match self.tool_index.get(&call.name).copied() {
                Some(idx) => self.servers[idx].connection.call_tool(call).await,
                None => tool_result_message(
                    call.id.clone(),
                    format!("unknown MCP tool: {}", call.name),
                    true,
                ),
            };
            results.push(msg);
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn start_mock_mcp_server(
        responses: Vec<serde_json::Value>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");

        let resps = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
            responses,
        )));

        let handle = tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };
                let resps = resps.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    let resp = resps
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or(serde_json::json!({"jsonrpc":"2.0","error":{"code":-1,"message":"no more responses"}}));
                    let body = serde_json::to_string(&resp).unwrap();
                    let http = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(http.as_bytes()).await;
                });
            }
        });

        (url, handle)
    }

    #[tokio::test]
    async fn discover_tools_parses_response() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "tools": [
                    {
                        "name": "create_pull_request",
                        "description": "Create a pull request",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "title": {"type": "string"},
                                "body": {"type": "string"}
                            },
                            "required": ["title"]
                        }
                    },
                    {
                        "name": "list_issues",
                        "description": "List issues",
                        "inputSchema": {"type": "object"}
                    }
                ]
            }
        });

        let (url, _handle) = start_mock_mcp_server(vec![response]).await;
        let mut conn = McpConnection::new(url, "test-token".into());
        let tools = conn.discover_tools(None).await.unwrap();

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "create_pull_request");
        assert_eq!(tools[0].description, "Create a pull request");
        // Verify inputSchema mapped to parameters (not just present, but correct structure)
        assert_eq!(tools[0].parameters["properties"]["title"]["type"], "string");
        assert_eq!(tools[0].parameters["required"][0], "title");
        assert_eq!(tools[1].name, "list_issues");
        assert_eq!(tools[1].parameters["type"], "object");
    }

    #[tokio::test]
    async fn discover_tools_filters_by_allowlist() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "tools": [
                    {"name": "tool_a", "description": "A", "inputSchema": {"type": "object"}},
                    {"name": "tool_b", "description": "B", "inputSchema": {"type": "object"}},
                    {"name": "tool_c", "description": "C", "inputSchema": {"type": "object"}}
                ]
            }
        });

        let (url, _handle) = start_mock_mcp_server(vec![response]).await;
        let mut conn = McpConnection::new(url, "test-token".into());

        let allow = vec!["tool_a".to_string(), "tool_c".to_string()];
        let tools = conn.discover_tools(Some(&allow)).await.unwrap();

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "tool_a");
        assert_eq!(tools[1].name, "tool_c");
        // Verify excluded tool is actually absent
        assert!(
            !tools.iter().any(|t| t.name == "tool_b"),
            "tool_b should be filtered out by allowlist"
        );
    }

    #[tokio::test]
    async fn discover_tools_empty_allowlist_returns_none() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "tools": [
                    {"name": "tool_a", "description": "A", "inputSchema": {"type": "object"}}
                ]
            }
        });

        let (url, _handle) = start_mock_mcp_server(vec![response]).await;
        let mut conn = McpConnection::new(url, "test-token".into());

        let allow: Vec<String> = vec![];
        let tools = conn.discover_tools(Some(&allow)).await.unwrap();
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn call_tool_returns_message() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [
                    {"type": "text", "text": "PR #42 created successfully"}
                ]
            }
        });

        let (url, _handle) = start_mock_mcp_server(vec![response]).await;
        let mut conn = McpConnection::new(url, "test-token".into());

        let tool_call = ToolCall {
            id: "tc-1".into(),
            name: "create_pull_request".into(),
            input: serde_json::json!({"title": "Fix bug"}),
        };
        let msg = conn.call_tool(&tool_call).await;

        assert_eq!(msg.role, "tool");
        assert_eq!(msg.tool_call_id.as_deref(), Some("tc-1"));
        assert_eq!(
            msg.content.as_ref().and_then(|v| v.as_str()),
            Some("PR #42 created successfully")
        );
        assert!(msg.is_error.is_none());
    }

    #[tokio::test]
    async fn call_tool_server_error_returns_is_error() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {
                "code": -32600,
                "message": "tool not found"
            }
        });

        let (url, _handle) = start_mock_mcp_server(vec![response]).await;
        let mut conn = McpConnection::new(url, "test-token".into());

        let tool_call = ToolCall {
            id: "tc-2".into(),
            name: "nonexistent".into(),
            input: serde_json::json!({}),
        };
        let msg = conn.call_tool(&tool_call).await;

        assert_eq!(msg.role, "tool");
        assert_eq!(msg.tool_call_id.as_deref(), Some("tc-2"));
        assert_eq!(msg.is_error, Some(true));
        let content_str = msg.content.as_ref().and_then(|v| v.as_str()).unwrap();
        assert!(content_str.contains("tool not found"), "{content_str}");
    }

    #[tokio::test]
    async fn manager_is_mcp_tool_routing() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "tools": [
                    {"name": "mcp_tool_1", "description": "T1", "inputSchema": {"type": "object"}},
                    {"name": "mcp_tool_2", "description": "T2", "inputSchema": {"type": "object"}}
                ]
            }
        });

        let (url, _handle) = start_mock_mcp_server(vec![response]).await;

        let configs = vec![ResolvedMcp {
            name: "test-server".into(),
            url,
            auth_env: "TEST_TOKEN".into(),
            tool_allowlist: None,
        }];

        let mut manager = McpManager::new(configs);
        manager.initialize().await.unwrap();

        assert!(manager.is_mcp_tool("mcp_tool_1"));
        assert!(manager.is_mcp_tool("mcp_tool_2"));
        assert!(!manager.is_mcp_tool("local_tool"));
        assert_eq!(manager.mcp_tools().len(), 2);
    }

    #[tokio::test]
    async fn manager_execute_tool_calls_concurrent() {
        let response1 = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"tools": [
                {"name": "tool_a", "description": "A", "inputSchema": {"type": "object"}}
            ]}
        });
        let call_response1 = serde_json::json!({
            "jsonrpc": "2.0", "id": 2,
            "result": {"content": [{"type": "text", "text": "result_a"}]}
        });
        let call_response2 = serde_json::json!({
            "jsonrpc": "2.0", "id": 3,
            "result": {"content": [{"type": "text", "text": "result_a_again"}]}
        });

        let (url, _handle) =
            start_mock_mcp_server(vec![response1, call_response1, call_response2]).await;

        let configs = vec![ResolvedMcp {
            name: "server".into(),
            url,
            auth_env: "TEST_TOKEN".into(),
            tool_allowlist: None,
        }];

        let mut manager = McpManager::new(configs);
        manager.initialize().await.unwrap();

        let calls = vec![
            ToolCall {
                id: "tc-1".into(),
                name: "tool_a".into(),
                input: serde_json::json!({"x": 1}),
            },
            ToolCall {
                id: "tc-2".into(),
                name: "tool_a".into(),
                input: serde_json::json!({"x": 2}),
            },
        ];

        let results = manager.execute_tool_calls(&calls).await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].tool_call_id.as_deref(), Some("tc-1"));
        assert_eq!(results[1].tool_call_id.as_deref(), Some("tc-2"));
        // Verify actual content was returned (not empty or swapped)
        let c0 = results[0].content.as_ref().unwrap().as_str().unwrap();
        let c1 = results[1].content.as_ref().unwrap().as_str().unwrap();
        assert!(c0.contains("result_a"), "first result content: {c0}");
        assert!(c1.contains("result_a"), "second result content: {c1}");
        assert!(results[0].is_error.is_none(), "should not be error");
        assert!(results[1].is_error.is_none(), "should not be error");
    }

    #[tokio::test]
    async fn manager_unknown_tool_returns_error() {
        let configs: Vec<ResolvedMcp> = vec![];
        let mut manager = McpManager::new(configs);

        let calls = vec![ToolCall {
            id: "tc-1".into(),
            name: "nonexistent".into(),
            input: serde_json::json!({}),
        }];

        let results = manager.execute_tool_calls(&calls).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].is_error, Some(true));
    }

    #[test]
    fn parse_sse_response() {
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n";
        let result = parse_sse_jsonrpc(body).unwrap();
        assert!(
            result["tools"].is_array(),
            "tools should be an array, got: {result}"
        );
    }

    #[test]
    fn parse_json_response() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let result = parse_json_jsonrpc(body).unwrap();
        assert!(
            result["tools"].is_array(),
            "tools should be an array, got: {result}"
        );
    }

    #[test]
    fn parse_json_error_response() {
        let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"bad request"}}"#;
        let err = parse_json_jsonrpc(body).unwrap_err();
        assert!(err.contains("bad request"), "{err}");
    }

    #[test]
    fn extract_multi_content_blocks() {
        let result = serde_json::json!({
            "content": [
                {"type": "text", "text": "line 1"},
                {"type": "text", "text": "line 2"}
            ]
        });
        let text = extract_content_text(&result);
        assert_eq!(text, "line 1\nline 2");
    }

    // Mock server that drops the Nth connection (0-indexed) to simulate connection failure
    async fn start_dropping_mock(
        drop_indices: Vec<usize>,
        responses: Vec<serde_json::Value>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");

        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resps = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
            responses,
        )));

        let handle = tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };
                let idx = call_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let should_drop = drop_indices.contains(&idx);
                let resps = resps.clone();

                tokio::spawn(async move {
                    if should_drop {
                        // Read the request then drop without responding
                        let mut buf = vec![0u8; 8192];
                        let _ = stream.read(&mut buf).await;
                        drop(stream);
                        return;
                    }

                    let mut buf = vec![0u8; 8192];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }

                    let resp = resps
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or(serde_json::json!({"jsonrpc":"2.0","error":{"code":-1,"message":"no response"}}));
                    let body = serde_json::to_string(&resp).unwrap();
                    let http = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(http.as_bytes()).await;
                });
            }
        });

        (url, handle)
    }

    #[tokio::test]
    async fn reconnect_on_connection_drop_succeeds() {
        // Connection 0: tools/list (success)
        // Connection 1: tools/call (DROPPED — simulates network failure)
        // Connection 2: tools/call retry (success)
        let tools_list = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"tools": [
                {"name": "search", "description": "S", "inputSchema": {"type": "object"}}
            ]}
        });
        let call_success = serde_json::json!({
            "jsonrpc": "2.0", "id": 2,
            "result": {"content": [{"type": "text", "text": "retry succeeded"}]}
        });

        let (url, _handle) = start_dropping_mock(vec![1], vec![tools_list, call_success]).await;

        let mut conn = McpConnection::new(url, "test-token".into());
        conn.discover_tools(None).await.unwrap();
        // After discover, connected == true

        let tool_call = ToolCall {
            id: "tc-retry".into(),
            name: "search".into(),
            input: serde_json::json!({"q": "test"}),
        };
        let msg = conn.call_tool(&tool_call).await;

        // Should succeed via retry
        assert!(msg.is_error.is_none(), "should succeed on retry, got error");
        assert_eq!(
            msg.content.as_ref().unwrap().as_str(),
            Some("retry succeeded")
        );
        assert_eq!(msg.tool_call_id.as_deref(), Some("tc-retry"));
    }

    #[tokio::test]
    async fn reconnect_fails_returns_error() {
        // Connection 0: tools/list (success)
        // Connection 1: tools/call (DROPPED)
        // Connection 2: tools/call retry (ALSO DROPPED)
        let tools_list = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"tools": [
                {"name": "search", "description": "S", "inputSchema": {"type": "object"}}
            ]}
        });

        let (url, _handle) = start_dropping_mock(vec![1, 2], vec![tools_list]).await;

        let mut conn = McpConnection::new(url, "test-token".into());
        conn.discover_tools(None).await.unwrap();

        let tool_call = ToolCall {
            id: "tc-fail".into(),
            name: "search".into(),
            input: serde_json::json!({}),
        };
        let msg = conn.call_tool(&tool_call).await;

        assert_eq!(
            msg.is_error,
            Some(true),
            "should be error after retry fails"
        );
        assert_eq!(msg.tool_call_id.as_deref(), Some("tc-fail"));
    }
}

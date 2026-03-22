pub mod conversation;
pub mod init;
pub mod mcp;
pub mod profile;
pub mod protocol;
pub mod provider;
pub mod streaming;

use conversation::ConversationLog;
use mcp::McpManager;
use profile::AgentProfile;
use protocol::{
    build_error, build_final_response, build_notification, send_line, validate_request, Message,
    StopReason, StreamData, ToolCall, ValidatedRequest,
};
use provider::{collect_text, collect_tool_calls, LlmProvider, ProviderConfig, StreamEvent};

use futures::StreamExt;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::RwLock;

pub type ProfileMap = Arc<HashMap<String, AgentProfile>>;
pub type ConversationMap = Arc<RwLock<HashMap<String, ConversationLog>>>;
pub type ProviderMap = Arc<HashMap<String, Box<dyn LlmProvider>>>;
pub type McpManagerMap = Arc<RwLock<HashMap<String, McpManager>>>;

async fn write_json_line(
    writer: &mut (impl AsyncWriteExt + Unpin),
    value: &impl serde::Serialize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    writer
        .write_all(&send_line(&serde_json::to_string(value)?))
        .await?;
    writer.flush().await?;
    Ok(())
}

fn none_if_empty<T>(v: Vec<T>) -> Option<Vec<T>> {
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

async fn send_notification(
    writer: &mut (impl AsyncWriteExt + Unpin),
    data: StreamData,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    write_json_line(writer, &build_notification("content", data)).await
}

// --- LLM call (extracted from old call_provider_and_respond) ---

struct LlmResult {
    stop_reason: StopReason,
    content: Option<String>,
    tool_calls: Vec<ToolCall>,
}

async fn call_llm(
    writer: &mut (impl AsyncWriteExt + Unpin),
    conversation: &mut ConversationLog,
    provider: &dyn LlmProvider,
    config: &ProviderConfig,
) -> Result<LlmResult, Box<dyn std::error::Error + Send + Sync>> {
    let tools = conversation.tools();
    let history = conversation.history();
    let system = conversation.system_prompt();

    let mut stream = provider
        .call(history, system, &tools, config)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

    let mut events: Vec<StreamEvent> = Vec::new();

    while let Some(result) = stream.next().await {
        match result {
            Ok(event) => {
                if let Some(data) = event.to_stream_data() {
                    send_notification(writer, data).await?;
                }
                events.push(event);
            }
            Err(e) => {
                return Err(format!("stream error: {e}").into());
            }
        }
    }

    let stop_reason_str = events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Done { stop_reason } => Some(stop_reason.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "end_turn".into());

    let stop_reason = StopReason::from_str_lossy(&stop_reason_str);
    let tool_calls = collect_tool_calls(&events);
    let content = collect_text(&events);

    let assistant_msg = Message {
        role: "assistant".into(),
        content: content.clone().map(serde_json::Value::String),
        tool_calls: none_if_empty(tool_calls.clone()),
        tool_call_id: None,
        is_error: None,
    };
    conversation.append(assistant_msg)?;

    Ok(LlmResult {
        stop_reason,
        content,
        tool_calls,
    })
}

// --- Pending MCP state (stored between turns when mixed local+MCP calls) ---

struct PendingMcpState {
    mcp_results: HashMap<String, Message>,
    call_order: Vec<String>,
}

impl PendingMcpState {
    fn new() -> Self {
        Self {
            mcp_results: HashMap::new(),
            call_order: Vec::new(),
        }
    }

    fn has_pending(&self) -> bool {
        !self.mcp_results.is_empty()
    }

    fn set(&mut self, results: Vec<Message>, call_order: Vec<String>) {
        self.mcp_results.clear();
        for msg in results {
            if let Some(id) = &msg.tool_call_id {
                self.mcp_results.insert(id.clone(), msg);
            }
        }
        self.call_order = call_order;
    }

    fn take(&mut self) -> (HashMap<String, Message>, Vec<String>) {
        let results = std::mem::take(&mut self.mcp_results);
        let order = std::mem::take(&mut self.call_order);
        (results, order)
    }
}

fn interleave_results(
    mut mcp_results: HashMap<String, Message>,
    local_messages: Vec<Message>,
    call_order: Vec<String>,
) -> Vec<Message> {
    let mut local_map: HashMap<String, Message> = HashMap::new();
    for msg in local_messages {
        if let Some(id) = &msg.tool_call_id {
            local_map.insert(id.clone(), msg);
        }
    }

    let mut ordered = Vec::with_capacity(call_order.len());
    for tc_id in &call_order {
        if let Some(msg) = mcp_results.remove(tc_id) {
            ordered.push(msg);
        } else if let Some(msg) = local_map.remove(tc_id) {
            ordered.push(msg);
        }
    }

    ordered
}

// --- Turn context (bundles args to avoid too_many_arguments) ---

struct TurnContext<'a> {
    conversation: &'a mut ConversationLog,
    provider: &'a dyn LlmProvider,
    config: &'a ProviderConfig,
    mcp_manager: &'a mut McpManager,
    pending: &'a mut PendingMcpState,
}

// --- Turn handling ---

async fn handle_turn(
    id: u64,
    params: protocol::TurnRequest,
    writer: &mut (impl AsyncWriteExt + Unpin),
    ctx: &mut TurnContext<'_>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(system) = params.system {
        ctx.conversation.set_system_prompt(system);
    }
    if let Some(tools) = params.tools {
        ctx.conversation.set_tools(tools);
    }

    if ctx.pending.has_pending() {
        let (mcp_results, call_order) = ctx.pending.take();
        let ordered = interleave_results(mcp_results, params.messages, call_order);
        ctx.conversation.append_many(ordered)?;
    } else {
        ctx.conversation.append_many(params.messages)?;
    }

    if !ctx.mcp_manager.is_initialized() && ctx.mcp_manager.has_servers() {
        match ctx.mcp_manager.initialize().await {
            Ok(()) => {
                let current_tools = ctx.conversation.tools();
                let local_names: std::collections::HashSet<&str> =
                    current_tools.iter().map(|t| t.name.as_str()).collect();

                let mcp_tools: Vec<protocol::ToolDefinition> = ctx
                    .mcp_manager
                    .mcp_tools()
                    .iter()
                    .filter(|t| {
                        if local_names.contains(t.name.as_str()) {
                            tracing::warn!(
                                "MCP tool '{}' conflicts with local tool, local wins",
                                t.name
                            );
                            false
                        } else {
                            true
                        }
                    })
                    .cloned()
                    .collect();

                ctx.conversation.set_mcp_tools(mcp_tools);
            }
            Err(e) => {
                write_json_line(writer, &build_error(id, -32603, format!("MCP init: {e}"))).await?;
                return Ok(());
            }
        }
    }

    loop {
        let result = match call_llm(writer, ctx.conversation, ctx.provider, ctx.config).await {
            Ok(r) => r,
            Err(e) => {
                write_json_line(writer, &build_error(id, -32603, format!("{e}"))).await?;
                return Ok(());
            }
        };

        match result.stop_reason {
            StopReason::EndTurn | StopReason::MaxTokens => {
                let resp = build_final_response(
                    id,
                    result.stop_reason,
                    none_if_empty(result.tool_calls),
                    result.content,
                );
                write_json_line(writer, &resp).await?;
                return Ok(());
            }
            StopReason::ToolUse => {
                let (mcp_calls, local_calls): (Vec<ToolCall>, Vec<ToolCall>) = result
                    .tool_calls
                    .into_iter()
                    .partition(|tc| ctx.mcp_manager.is_mcp_tool(&tc.name));

                let mcp_results = if !mcp_calls.is_empty() {
                    ctx.mcp_manager.execute_tool_calls(&mcp_calls).await
                } else {
                    Vec::new()
                };

                if local_calls.is_empty() {
                    // All MCP — append results, loop internally
                    ctx.conversation.append_many(mcp_results)?;
                    continue;
                }

                if mcp_calls.is_empty() {
                    // All local — return to runtime
                    let resp = build_final_response(
                        id,
                        StopReason::ToolUse,
                        Some(local_calls),
                        result.content,
                    );
                    write_json_line(writer, &resp).await?;
                    return Ok(());
                }

                // Mixed — store MCP results, send local to runtime
                let call_order: Vec<String> = mcp_calls
                    .iter()
                    .chain(local_calls.iter())
                    .map(|tc| tc.id.clone())
                    .collect();

                ctx.pending.set(mcp_results, call_order);

                let resp = build_final_response(
                    id,
                    StopReason::ToolUse,
                    Some(local_calls),
                    result.content,
                );
                write_json_line(writer, &resp).await?;
                return Ok(());
            }
        }
    }
}

pub async fn handle_connection(
    stream: tokio::net::UnixStream,
    profile_name: String,
    profiles: ProfileMap,
    conversations: ConversationMap,
    providers: ProviderMap,
    mcp_managers: McpManagerMap,
    logs_base_dir: PathBuf,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    let profile = profiles
        .get(&profile_name)
        .expect("profile must exist in map");

    let provider = providers
        .get(&profile.llm.provider)
        .ok_or_else(|| format!("unknown provider: {}", profile.llm.provider))?;

    let api_key = std::env::var(&profile.llm.api_key_env).unwrap_or_default();
    let config = ProviderConfig {
        model: profile.llm.model.clone(),
        api_key,
        max_tokens: profile.llm.max_tokens,
    };

    let mut convos = conversations.write().await;
    let conversation = convos
        .entry(profile_name.clone())
        .or_insert_with(|| ConversationLog::new(&logs_base_dir.join(&profile_name)));

    let mut mcp_mgrs = mcp_managers.write().await;
    let mcp_manager = mcp_mgrs
        .entry(profile_name.clone())
        .or_insert_with(|| McpManager::new(profile.mcp_servers.clone()));

    let mut pending = PendingMcpState::new();

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("connection read error for {profile_name}: {e}");
                break;
            }
        }

        match validate_request(&line) {
            Ok(ValidatedRequest::Turn { id, params }) => {
                let mut ctx = TurnContext {
                    conversation,
                    provider: provider.as_ref(),
                    config: &config,
                    mcp_manager,
                    pending: &mut pending,
                };
                handle_turn(id, params, &mut writer, &mut ctx).await?;
            }
            Err(err) => {
                write_json_line(&mut writer, &err).await?;
            }
        }
    }

    Ok(())
}

pub fn bind_agent_socket(
    path: &Path,
) -> Result<UnixListener, Box<dyn std::error::Error + Send + Sync>> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

pub async fn run_daemon(
    listeners: Vec<(String, UnixListener)>,
    profiles: ProfileMap,
    conversations: ConversationMap,
    providers: ProviderMap,
    mcp_managers: McpManagerMap,
    logs_base_dir: PathBuf,
) {
    for (profile_name, listener) in listeners {
        let name = profile_name;
        let profs = profiles.clone();
        let convos = conversations.clone();
        let provs = providers.clone();
        let mcps = mcp_managers.clone();
        let logs_dir = logs_base_dir.clone();

        tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(e) => {
                        tracing::error!("accept error on {name}: {e}");
                        continue;
                    }
                };

                let n = name.clone();
                let p = profs.clone();
                let c = convos.clone();
                let pv = provs.clone();
                let m = mcps.clone();
                let ld = logs_dir.clone();

                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, n, p, c, pv, m, ld).await {
                        tracing::error!("connection error: {e}");
                    }
                });
            }
        });
    }

    std::future::pending::<()>().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_result_msg(tc_id: &str, content: &str) -> Message {
        mcp::tool_result_message(tc_id.into(), content.into(), false)
    }

    // --- interleave_results ---

    #[test]
    fn interleave_preserves_call_order() {
        let mut mcp = HashMap::new();
        mcp.insert("mcp-1".into(), tool_result_msg("mcp-1", "mcp result 1"));
        mcp.insert("mcp-2".into(), tool_result_msg("mcp-2", "mcp result 2"));

        let local = vec![tool_result_msg("local-1", "local result 1")];

        let order = vec!["mcp-1".into(), "local-1".into(), "mcp-2".into()];

        let result = interleave_results(mcp, local, order);

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].tool_call_id.as_deref(), Some("mcp-1"));
        assert_eq!(result[1].tool_call_id.as_deref(), Some("local-1"));
        assert_eq!(result[2].tool_call_id.as_deref(), Some("mcp-2"));

        // Verify content to catch swaps
        assert_eq!(
            result[0].content.as_ref().unwrap().as_str(),
            Some("mcp result 1")
        );
        assert_eq!(
            result[1].content.as_ref().unwrap().as_str(),
            Some("local result 1")
        );
        assert_eq!(
            result[2].content.as_ref().unwrap().as_str(),
            Some("mcp result 2")
        );
    }

    #[test]
    fn interleave_matches_by_id_not_position() {
        let mut mcp = HashMap::new();
        mcp.insert("mcp-1".into(), tool_result_msg("mcp-1", "mcp"));

        // Local messages in reverse order relative to call_order
        let local = vec![
            tool_result_msg("local-2", "local 2"),
            tool_result_msg("local-1", "local 1"),
        ];

        let order = vec!["local-1".into(), "mcp-1".into(), "local-2".into()];

        let result = interleave_results(mcp, local, order);

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].tool_call_id.as_deref(), Some("local-1"));
        assert_eq!(
            result[0].content.as_ref().unwrap().as_str(),
            Some("local 1")
        );
        assert_eq!(result[1].tool_call_id.as_deref(), Some("mcp-1"));
        assert_eq!(result[2].tool_call_id.as_deref(), Some("local-2"));
        assert_eq!(
            result[2].content.as_ref().unwrap().as_str(),
            Some("local 2")
        );
    }

    #[test]
    fn interleave_skips_missing_ids() {
        let mut mcp = HashMap::new();
        mcp.insert("mcp-1".into(), tool_result_msg("mcp-1", "mcp"));

        let local = vec![tool_result_msg("local-1", "local")];

        // "ghost" is in call_order but not in either map
        let order = vec!["mcp-1".into(), "ghost".into(), "local-1".into()];

        let result = interleave_results(mcp, local, order);

        assert_eq!(result.len(), 2, "ghost id should be skipped");
        assert_eq!(result[0].tool_call_id.as_deref(), Some("mcp-1"));
        assert_eq!(result[1].tool_call_id.as_deref(), Some("local-1"));
    }

    #[test]
    fn interleave_empty_inputs() {
        let result = interleave_results(HashMap::new(), vec![], vec![]);
        assert!(result.is_empty());
    }

    // --- PendingMcpState ---

    #[test]
    fn pending_new_is_empty() {
        let state = PendingMcpState::new();
        assert!(!state.has_pending());
    }

    #[test]
    fn pending_set_then_has_pending() {
        let mut state = PendingMcpState::new();
        state.set(vec![tool_result_msg("tc-1", "result")], vec!["tc-1".into()]);
        assert!(state.has_pending());
    }

    #[test]
    fn pending_take_clears_state() {
        let mut state = PendingMcpState::new();
        state.set(
            vec![tool_result_msg("tc-1", "result")],
            vec!["tc-1".into(), "tc-2".into()],
        );

        let (results, order) = state.take();

        assert!(!state.has_pending(), "take should clear pending state");
        assert_eq!(results.len(), 1);
        assert!(results.contains_key("tc-1"));
        assert_eq!(order, vec!["tc-1", "tc-2"]);
    }

    #[test]
    fn pending_set_indexes_by_tool_call_id() {
        let mut state = PendingMcpState::new();
        state.set(
            vec![
                tool_result_msg("tc-a", "result a"),
                tool_result_msg("tc-b", "result b"),
            ],
            vec!["tc-a".into(), "tc-b".into()],
        );

        let (results, _) = state.take();

        assert_eq!(results.len(), 2);
        let a = &results["tc-a"];
        assert_eq!(a.content.as_ref().unwrap().as_str(), Some("result a"));
        let b = &results["tc-b"];
        assert_eq!(b.content.as_ref().unwrap().as_str(), Some("result b"));
    }
}

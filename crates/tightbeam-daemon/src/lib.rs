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
    build_delivered_notification, build_disconnect_notification, build_end_turn_notification,
    build_error, build_final_response, build_human_message_notification, build_notification,
    build_send_response, validate_request, ContentBlock, Message, SendParams, StopReason, ToolCall,
    ValidatedRequest,
};
use provider::{collect_text, collect_tool_calls, LlmProvider, ProviderConfig, StreamEvent};

use futures::StreamExt;
use std::collections::{HashMap, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tightbeam_protocol::framing::{read_frame, write_frame};
use tokio::io::AsyncWriteExt;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot, Mutex as TokioMutex, RwLock};

pub type ProfileMap = Arc<HashMap<String, AgentProfile>>;
pub type ConversationMap = Arc<RwLock<HashMap<String, ConversationLog>>>;
pub type ProviderMap = Arc<HashMap<String, Box<dyn LlmProvider>>>;
pub type McpManagerMap = Arc<RwLock<HashMap<String, McpManager>>>;
pub type HumanMessageSenderMap = Arc<RwLock<HashMap<String, mpsc::Sender<HumanMessageDelivery>>>>;
pub type AgentStateMap = Arc<HashMap<String, Arc<TokioMutex<AgentState>>>>;

type SubscriberWriter = Arc<TokioMutex<OwnedWriteHalf>>;

pub struct HumanMessageDelivery {
    content: Vec<ContentBlock>,
    subscriber: Option<SubscriberWriter>,
    delivered_tx: Option<oneshot::Sender<()>>,
}

pub struct AgentState {
    busy: bool,
    queue: VecDeque<HumanMessageDelivery>,
    subscribers: Vec<SubscriberWriter>,
}

impl AgentState {
    pub fn new() -> Self {
        Self {
            busy: false,
            queue: VecDeque::new(),
            subscribers: Vec::new(),
        }
    }
}

impl Default for AgentState {
    fn default() -> Self {
        Self::new()
    }
}

// --- Framing helpers ---

async fn write_framed_json(
    writer: &mut (impl AsyncWriteExt + Unpin),
    value: &impl serde::Serialize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let payload = serde_json::to_vec(value)?;
    write_frame(writer, &payload).await?;
    Ok(())
}

fn none_if_empty<T>(v: Vec<T>) -> Option<Vec<T>> {
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

// --- Subscriber broadcast ---

async fn broadcast_to_subscribers(subscribers: &[SubscriberWriter], value: &serde_json::Value) {
    let payload = match serde_json::to_vec(value) {
        Ok(p) => p,
        Err(_) => return,
    };
    for sub in subscribers {
        let mut w = sub.lock().await;
        let _ = write_frame(&mut *w, &payload).await;
    }
}

async fn get_subscribers(state: &Arc<TokioMutex<AgentState>>) -> Vec<SubscriberWriter> {
    state.lock().await.subscribers.clone()
}

// --- LLM call ---

struct LlmResult {
    stop_reason: StopReason,
    content: Option<Vec<ContentBlock>>,
    tool_calls: Vec<ToolCall>,
}

async fn call_llm(
    writer: &mut (impl AsyncWriteExt + Unpin),
    subscribers: &[SubscriberWriter],
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
                    let notif = build_notification("content", data.clone());
                    write_framed_json(writer, &notif).await?;
                    if data.data_type == "text" {
                        let notif_value = serde_json::to_value(notif).unwrap();
                        broadcast_to_subscribers(subscribers, &notif_value).await;
                    }
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
    let content = collect_text(&events).map(ContentBlock::text_content);

    let assistant_msg = Message {
        role: "assistant".into(),
        content: content.clone(),
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
    subscribers: &'a [SubscriberWriter],
}

// --- Turn handling ---

async fn handle_turn(
    id: u64,
    params: protocol::TurnRequest,
    writer: &mut (impl AsyncWriteExt + Unpin),
    ctx: &mut TurnContext<'_>,
) -> Result<StopReason, Box<dyn std::error::Error + Send + Sync>> {
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
                write_framed_json(writer, &build_error(id, -32603, format!("MCP init: {e}")))
                    .await?;
                return Ok(StopReason::EndTurn);
            }
        }
    }

    loop {
        let result = match call_llm(
            writer,
            ctx.subscribers,
            ctx.conversation,
            ctx.provider,
            ctx.config,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                write_framed_json(writer, &build_error(id, -32603, format!("{e}"))).await?;
                return Ok(StopReason::EndTurn);
            }
        };

        match result.stop_reason {
            StopReason::EndTurn | StopReason::MaxTokens => {
                let stop = result.stop_reason.clone();
                let resp = build_final_response(
                    id,
                    result.stop_reason,
                    none_if_empty(result.tool_calls),
                    result.content,
                );
                write_framed_json(writer, &resp).await?;
                return Ok(stop);
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
                    ctx.conversation.append_many(mcp_results)?;
                    continue;
                }

                if !mcp_calls.is_empty() {
                    let call_order: Vec<String> = mcp_calls
                        .iter()
                        .chain(local_calls.iter())
                        .map(|tc| tc.id.clone())
                        .collect();
                    ctx.pending.set(mcp_results, call_order);
                }

                let resp = build_final_response(
                    id,
                    StopReason::ToolUse,
                    Some(local_calls),
                    result.content,
                );
                write_framed_json(writer, &resp).await?;
                return Ok(StopReason::ToolUse);
            }
        }
    }
}

// --- deliver_human_message: write to runtime, register subscriber, signal delivery ---

async fn deliver_human_message(
    delivery: HumanMessageDelivery,
    writer: &mut OwnedWriteHalf,
    agent_state: &Arc<TokioMutex<AgentState>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let notif = build_human_message_notification(&delivery.content);
    write_framed_json(writer, &notif).await?;

    let mut state = agent_state.lock().await;
    if let Some(sub) = delivery.subscriber {
        state.subscribers.push(sub);
    }
    state.busy = true;
    drop(state);

    if let Some(tx) = delivery.delivered_tx {
        let _ = tx.send(());
    }
    Ok(())
}

// --- finish_turn: broadcast end_turn, drain queue ---

async fn finish_turn(
    agent_state: &Arc<TokioMutex<AgentState>>,
    writer: &mut OwnedWriteHalf,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (subs, next) = {
        let mut state = agent_state.lock().await;
        let subs = state.subscribers.clone();
        state.subscribers.clear();
        state.busy = false;
        (subs, state.queue.pop_front())
    };

    let notif = build_end_turn_notification();
    broadcast_to_subscribers(&subs, &notif).await;

    if let Some(delivery) = next {
        deliver_human_message(delivery, writer, agent_state).await?;
    }

    Ok(())
}

// --- cleanup_agent: called on runtime disconnect ---

async fn cleanup_agent(
    agent: &str,
    senders: &HumanMessageSenderMap,
    agent_state: &Arc<TokioMutex<AgentState>>,
) {
    senders.write().await.remove(agent);

    let subs = {
        let mut state = agent_state.lock().await;
        state.busy = false;
        state.queue.clear();
        std::mem::take(&mut state.subscribers)
    };

    let notif = build_disconnect_notification();
    broadcast_to_subscribers(&subs, &notif).await;
}

// --- Connection routing ---

#[allow(clippy::too_many_arguments)]
pub async fn handle_connection(
    stream: tokio::net::UnixStream,
    profile_name: String,
    profiles: ProfileMap,
    conversations: ConversationMap,
    providers: ProviderMap,
    mcp_managers: McpManagerMap,
    agent_state: Arc<TokioMutex<AgentState>>,
    human_msg_senders: HumanMessageSenderMap,
    logs_base_dir: PathBuf,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut reader, mut writer) = stream.into_split();

    let first_frame = match read_frame(&mut reader).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(()),
        Err(e) => {
            tracing::warn!("connection read error for {profile_name}: {e}");
            return Ok(());
        }
    };

    let raw = String::from_utf8_lossy(&first_frame);
    match validate_request(&raw) {
        Ok(ValidatedRequest::Register) => {
            handle_runtime_connection(
                reader,
                writer,
                profile_name,
                profiles,
                conversations,
                providers,
                mcp_managers,
                agent_state,
                human_msg_senders,
                logs_base_dir,
            )
            .await
        }
        Ok(ValidatedRequest::Send { id, params }) => {
            handle_send_connection(
                id,
                params,
                reader,
                writer,
                profile_name,
                agent_state,
                human_msg_senders,
            )
            .await
        }
        Ok(ValidatedRequest::Turn { id, .. }) => {
            write_framed_json(
                &mut writer,
                &build_error(id, -32600, "register first".into()),
            )
            .await?;
            Ok(())
        }
        Err(err) => {
            write_framed_json(&mut writer, &err).await?;
            Ok(())
        }
    }
}

// --- Runtime connection handler ---

#[allow(clippy::too_many_arguments)]
async fn handle_runtime_connection(
    mut reader: OwnedReadHalf,
    mut writer: OwnedWriteHalf,
    profile_name: String,
    profiles: ProfileMap,
    conversations: ConversationMap,
    providers: ProviderMap,
    mcp_managers: McpManagerMap,
    agent_state: Arc<TokioMutex<AgentState>>,
    human_msg_senders: HumanMessageSenderMap,
    logs_base_dir: PathBuf,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let profile = profiles
        .get(&profile_name)
        .expect("profile must exist in map");

    let provider = providers
        .get(&profile.llm.provider)
        .ok_or_else(|| format!("unknown provider: {}", profile.llm.provider))?;

    let api_key = profile.llm.api_key.clone();
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

    let (tx, mut rx) = mpsc::channel::<HumanMessageDelivery>(32);
    human_msg_senders
        .write()
        .await
        .insert(profile_name.clone(), tx);

    let mut pending = PendingMcpState::new();

    loop {
        tokio::select! {
            frame = read_frame(&mut reader) => {
                match frame {
                    Ok(Some(bytes)) => {
                        let raw = String::from_utf8_lossy(&bytes);
                        match validate_request(&raw) {
                            Ok(ValidatedRequest::Turn { id, params }) => {
                                let subs = get_subscribers(&agent_state).await;
                                let mut ctx = TurnContext {
                                    conversation,
                                    provider: provider.as_ref(),
                                    config: &config,
                                    mcp_manager,
                                    pending: &mut pending,
                                    subscribers: &subs,
                                };
                                let stop = handle_turn(id, params, &mut writer, &mut ctx).await?;
                                if matches!(stop, StopReason::EndTurn | StopReason::MaxTokens) {
                                    finish_turn(&agent_state, &mut writer).await?;
                                }
                            }
                            Ok(ValidatedRequest::Send { id, .. }) => {
                                write_framed_json(
                                    &mut writer,
                                    &build_error(id, -32600, "send not supported on runtime connection".into()),
                                ).await?;
                            }
                            Ok(ValidatedRequest::Register) => {}
                            Err(err) => {
                                write_framed_json(&mut writer, &err).await?;
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!("connection read error for {profile_name}: {e}");
                        break;
                    }
                }
            }
            delivery = rx.recv() => {
                if let Some(msg) = delivery {
                    deliver_human_message(msg, &mut writer, &agent_state).await?;
                }
            }
        }
    }

    cleanup_agent(&profile_name, &human_msg_senders, &agent_state).await;
    Ok(())
}

// --- Send connection handler ---

async fn handle_send_connection(
    id: u64,
    params: SendParams,
    mut reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
    profile_name: String,
    agent_state: Arc<TokioMutex<AgentState>>,
    human_msg_senders: HumanMessageSenderMap,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let sender = human_msg_senders.read().await.get(&profile_name).cloned();
    let sender = match sender {
        Some(s) => s,
        None => {
            let mut w = writer;
            write_framed_json(
                &mut w,
                &build_error(id, -32000, "agent runtime not connected".into()),
            )
            .await?;
            return Ok(());
        }
    };

    let sub_handle: SubscriberWriter = Arc::new(TokioMutex::new(writer));
    let (delivered_tx, delivered_rx) = oneshot::channel();

    let was_busy = {
        let mut state = agent_state.lock().await;
        let busy = state.busy;
        if !busy {
            state.busy = true;
        }
        busy
    };

    let delivery = HumanMessageDelivery {
        content: params.content,
        subscriber: Some(sub_handle.clone()),
        delivered_tx: Some(delivered_tx),
    };

    if was_busy {
        agent_state.lock().await.queue.push_back(delivery);
        {
            let mut w = sub_handle.lock().await;
            write_framed_json(&mut *w, &build_send_response(id, "queued")).await?;
        }
    } else if sender.send(delivery).await.is_err() {
        let mut state = agent_state.lock().await;
        state.busy = false;
        let mut w = sub_handle.lock().await;
        write_framed_json(
            &mut *w,
            &build_error(id, -32000, "agent runtime disconnected".into()),
        )
        .await?;
        return Ok(());
    } else {
        let mut w = sub_handle.lock().await;
        write_framed_json(&mut *w, &build_send_response(id, "delivered")).await?;
    }

    if was_busy {
        let _ = delivered_rx.await;
        let mut w = sub_handle.lock().await;
        write_framed_json(&mut *w, &build_delivered_notification()).await?;
    }

    // Keep task alive: read until subscriber disconnects
    while let Ok(Some(_)) = read_frame(&mut reader).await {}

    let mut state = agent_state.lock().await;
    state.subscribers.retain(|s| !Arc::ptr_eq(s, &sub_handle));

    Ok(())
}

// --- Socket binding ---

pub fn bind_agent_socket(
    path: &Path,
) -> Result<UnixListener, Box<dyn std::error::Error + Send + Sync>> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

// --- Daemon runner ---

pub async fn run_daemon(
    listeners: Vec<(String, UnixListener)>,
    profiles: ProfileMap,
    conversations: ConversationMap,
    providers: ProviderMap,
    mcp_managers: McpManagerMap,
    logs_base_dir: PathBuf,
) {
    let human_msg_senders: HumanMessageSenderMap = Arc::new(RwLock::new(HashMap::new()));

    let mut agent_state_map: HashMap<String, Arc<TokioMutex<AgentState>>> = HashMap::new();
    for (name, _) in &listeners {
        agent_state_map.insert(name.clone(), Arc::new(TokioMutex::new(AgentState::new())));
    }
    let agent_states: AgentStateMap = Arc::new(agent_state_map);

    for (profile_name, listener) in listeners {
        let name = profile_name;
        let profs = profiles.clone();
        let convos = conversations.clone();
        let provs = providers.clone();
        let mcps = mcp_managers.clone();
        let logs_dir = logs_base_dir.clone();
        let senders = human_msg_senders.clone();
        let states = agent_states.clone();

        tokio::spawn(async move {
            let agent_state = states.get(&name).expect("agent state must exist").clone();

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
                let s = senders.clone();
                let as_ = agent_state.clone();

                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, n, p, c, pv, m, as_, s, ld).await {
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
    use tightbeam_protocol::content_text;

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

        assert_eq!(content_text(&result[0].content), Some("mcp result 1"));
        assert_eq!(content_text(&result[1].content), Some("local result 1"));
        assert_eq!(content_text(&result[2].content), Some("mcp result 2"));
    }

    #[test]
    fn interleave_matches_by_id_not_position() {
        let mut mcp = HashMap::new();
        mcp.insert("mcp-1".into(), tool_result_msg("mcp-1", "mcp"));

        let local = vec![
            tool_result_msg("local-2", "local 2"),
            tool_result_msg("local-1", "local 1"),
        ];

        let order = vec!["local-1".into(), "mcp-1".into(), "local-2".into()];

        let result = interleave_results(mcp, local, order);

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].tool_call_id.as_deref(), Some("local-1"));
        assert_eq!(content_text(&result[0].content), Some("local 1"));
        assert_eq!(result[1].tool_call_id.as_deref(), Some("mcp-1"));
        assert_eq!(result[2].tool_call_id.as_deref(), Some("local-2"));
        assert_eq!(content_text(&result[2].content), Some("local 2"));
    }

    #[test]
    fn interleave_skips_missing_ids() {
        let mut mcp = HashMap::new();
        mcp.insert("mcp-1".into(), tool_result_msg("mcp-1", "mcp"));

        let local = vec![tool_result_msg("local-1", "local")];

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
        assert_eq!(content_text(&a.content), Some("result a"));
        let b = &results["tc-b"];
        assert_eq!(content_text(&b.content), Some("result b"));
    }
}

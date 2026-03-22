pub mod conversation;
pub mod init;
pub mod lifecycle;
pub mod profile;
pub mod protocol;
pub mod provider;
pub mod streaming;

use conversation::ConversationLog;
use lifecycle::IdleMap;
use profile::AgentProfile;
use protocol::{
    build_error, build_final_response, build_notification, send_line, validate_request, StreamData,
    ValidatedRequest,
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

async fn call_provider_and_respond(
    id: u64,
    writer: &mut (impl AsyncWriteExt + Unpin),
    conversation: &mut ConversationLog,
    provider: &dyn LlmProvider,
    config: &ProviderConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let tools = conversation.tools();
    let history = conversation.history();
    let system = conversation.system_prompt();

    let mut stream = match provider.call(history, system, tools, config).await {
        Ok(s) => s,
        Err(e) => {
            let err = build_error(id, -32603, e);
            writer
                .write_all(&send_line(&serde_json::to_string(&err)?))
                .await?;
            return Ok(());
        }
    };

    let mut events: Vec<StreamEvent> = Vec::new();

    while let Some(result) = stream.next().await {
        match result {
            Ok(event) => {
                match &event {
                    StreamEvent::ContentDelta { text } => {
                        let notif = build_notification(
                            "content",
                            StreamData {
                                data_type: "text".into(),
                                text: Some(text.clone()),
                                id: None,
                                name: None,
                                input: None,
                            },
                        );
                        writer
                            .write_all(&send_line(&serde_json::to_string(&notif)?))
                            .await?;
                        writer.flush().await?;
                    }
                    StreamEvent::ToolUseStart { id: tc_id, name } => {
                        let notif = build_notification(
                            "content",
                            StreamData {
                                data_type: "tool_use_start".into(),
                                text: None,
                                id: Some(tc_id.clone()),
                                name: Some(name.clone()),
                                input: None,
                            },
                        );
                        writer
                            .write_all(&send_line(&serde_json::to_string(&notif)?))
                            .await?;
                        writer.flush().await?;
                    }
                    StreamEvent::ToolUseInput { json } => {
                        let notif = build_notification(
                            "content",
                            StreamData {
                                data_type: "tool_use_input".into(),
                                text: Some(json.clone()),
                                id: None,
                                name: None,
                                input: None,
                            },
                        );
                        writer
                            .write_all(&send_line(&serde_json::to_string(&notif)?))
                            .await?;
                        writer.flush().await?;
                    }
                    StreamEvent::Done { .. } => {}
                }
                events.push(event);
            }
            Err(e) => {
                let err = build_error(id, -32603, format!("stream error: {e}"));
                writer
                    .write_all(&send_line(&serde_json::to_string(&err)?))
                    .await?;
                return Ok(());
            }
        }
    }

    let stop_reason = events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Done { stop_reason } => Some(stop_reason.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "end_turn".into());

    let tool_calls = collect_tool_calls(&events);
    let text = collect_text(&events);

    let tc_opt = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls.clone())
    };

    let resp = build_final_response(id, stop_reason, tc_opt, text.clone());
    writer
        .write_all(&send_line(&serde_json::to_string(&resp)?))
        .await?;
    writer.flush().await?;

    let assistant_msg = protocol::Message {
        role: "assistant".into(),
        content: text.map(serde_json::Value::String),
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        tool_call_id: None,
    };
    conversation.append(assistant_msg)?;

    Ok(())
}

async fn handle_llm_call(
    id: u64,
    params: protocol::LlmCallParams,
    writer: &mut (impl AsyncWriteExt + Unpin),
    conversation: &mut ConversationLog,
    provider: &dyn LlmProvider,
    config: &ProviderConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(system) = params.system {
        conversation.set_system_prompt(system);
    }
    conversation.append_many(params.messages)?;
    conversation.set_tools(params.tools);

    call_provider_and_respond(id, writer, conversation, provider, config).await
}

async fn handle_tool_result(
    id: u64,
    params: protocol::ToolResultParams,
    writer: &mut (impl AsyncWriteExt + Unpin),
    conversation: &mut ConversationLog,
    provider: &dyn LlmProvider,
    config: &ProviderConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let tool_msg = protocol::Message {
        role: "tool".into(),
        content: Some(serde_json::Value::String(params.result)),
        tool_calls: None,
        tool_call_id: Some(params.tool_call_id),
    };
    conversation.append(tool_msg)?;

    call_provider_and_respond(id, writer, conversation, provider, config).await
}

pub async fn handle_connection(
    stream: tokio::net::UnixStream,
    profile_name: String,
    profiles: ProfileMap,
    conversations: ConversationMap,
    providers: ProviderMap,
    idle_map: IdleMap,
    logs_base_dir: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    let profile = profiles
        .get(&profile_name)
        .expect("profile must exist in map");

    let provider = providers
        .get(&profile.provider.name)
        .ok_or_else(|| format!("unknown provider: {}", profile.provider.name))?;

    let api_key = std::env::var(&profile.provider.api_key_env).unwrap_or_default();
    let config = ProviderConfig {
        model: profile.provider.model.clone(),
        api_key,
        max_tokens: profile.provider.max_tokens,
    };

    let mut convos = conversations.write().await;
    let conversation = convos
        .entry(profile_name.clone())
        .or_insert_with(|| ConversationLog::new(&logs_base_dir.join(&profile.log.path)));

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

        lifecycle::touch(&idle_map, &profile_name).await;

        match validate_request(&line) {
            Ok(ValidatedRequest::LlmCall { id, params }) => {
                handle_llm_call(
                    id,
                    params,
                    &mut writer,
                    conversation,
                    provider.as_ref(),
                    &config,
                )
                .await?;
            }
            Ok(ValidatedRequest::ToolResult { id, params }) => {
                handle_tool_result(
                    id,
                    params,
                    &mut writer,
                    conversation,
                    provider.as_ref(),
                    &config,
                )
                .await?;
            }
            Err(err) => {
                writer
                    .write_all(&send_line(&serde_json::to_string(&err)?))
                    .await?;
            }
        }
    }

    Ok(())
}

pub fn bind_agent_socket(path: &Path) -> Result<UnixListener, Box<dyn std::error::Error>> {
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
    idle_map: IdleMap,
    logs_base_dir: PathBuf,
) {
    for (profile_name, listener) in listeners {
        let name = profile_name;
        let profs = profiles.clone();
        let convos = conversations.clone();
        let provs = providers.clone();
        let idles = idle_map.clone();
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
                let im = idles.clone();
                let ld = logs_dir.clone();

                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, n, p, c, pv, im, ld).await {
                        tracing::error!("connection error: {e}");
                    }
                });
            }
        });
    }

    std::future::pending::<()>().await;
}

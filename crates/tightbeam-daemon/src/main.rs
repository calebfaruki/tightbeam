use tightbeam_daemon::config::AgentConfig;
use tightbeam_daemon::{bind_agent_socket, run_daemon, Conversation, McpState, Profile, Provider};

use std::env;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use tightbeam_protocol::framing::{read_frame, write_frame};
use tightbeam_protocol::{LOGS_PATH, SOCKET_PATH};
use tokio::net::UnixStream;
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

fn logs_dir() -> &'static Path {
    Path::new(LOGS_PATH)
        .parent()
        .expect("LOGS_PATH must have a parent directory")
}

async fn send_command(args: &[String]) -> Result<(), String> {
    let message = args.join(" ");
    if message.is_empty() {
        return Err("usage: tightbeam-daemon send <message>".into());
    }

    let mut content: Vec<serde_json::Value> = Vec::new();
    content.push(serde_json::json!({"type": "text", "text": message}));

    let stream = UnixStream::connect(SOCKET_PATH)
        .await
        .map_err(|e| format!("cannot connect: {e}"))?;
    let (mut reader, mut writer) = stream.into_split();

    let rpc = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "send",
        "params": {"content": content}
    });
    let payload = serde_json::to_vec(&rpc).map_err(|e| format!("serialize error: {e}"))?;
    write_frame(&mut writer, &payload)
        .await
        .map_err(|e| format!("write error: {e}"))?;

    let resp_frame = read_frame(&mut reader)
        .await
        .map_err(|e| format!("read error: {e}"))?
        .ok_or("daemon disconnected")?;
    let resp: serde_json::Value =
        serde_json::from_slice(&resp_frame).map_err(|e| format!("invalid JSON: {e}"))?;

    if let Some(error) = resp.get("error") {
        let msg = error["message"].as_str().unwrap_or("unknown error");
        return Err(msg.to_string());
    }

    loop {
        let frame = read_frame(&mut reader)
            .await
            .map_err(|e| format!("read error: {e}"))?
            .ok_or("daemon disconnected")?;
        let raw = String::from_utf8_lossy(&frame);
        let value: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| format!("invalid JSON: {e}"))?;

        if let Some(method) = value.get("method").and_then(|m| m.as_str()) {
            match method {
                "delivered" => {}
                "end_turn" => {
                    println!();
                    break;
                }
                "output" => {
                    if let Some(text) = value["params"]["data"]["text"].as_str() {
                        print!("{text}");
                        let _ = std::io::stdout().flush();
                    }
                }
                "error" => {
                    let msg = value["params"]["message"]
                        .as_str()
                        .unwrap_or("unknown error");
                    return Err(msg.to_string());
                }
                _ => {}
            }
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let subcommand = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match subcommand {
        "version" => {
            println!("tightbeam-daemon {}", env!("CARGO_PKG_VERSION"));
        }
        "logs" => match std::fs::read_to_string(LOGS_PATH) {
            Ok(content) => print!("{content}"),
            Err(e) => {
                eprintln!("tightbeam: failed to read logs: {e}");
                std::process::exit(1);
            }
        },
        "send" => {
            if let Err(e) = send_command(&args[2..]).await {
                eprintln!("tightbeam: {e}");
                std::process::exit(1);
            }
        }
        "check" => match AgentConfig::load() {
            Ok(_) => println!("  ok"),
            Err(e) => {
                eprintln!("  FAIL {e}");
                std::process::exit(1);
            }
        },
        "start" => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::from_default_env().add_directive("info".parse().unwrap()),
                )
                .init();

            let agent_config = AgentConfig::load().unwrap_or_else(|e| {
                eprintln!("tightbeam: {e}");
                std::process::exit(1);
            });

            let mcp_count = agent_config.mcp_servers.len();
            tracing::info!("loaded LLM config, {mcp_count} MCP server(s)");

            let socket_path = Path::new(SOCKET_PATH);
            if let Some(parent) = socket_path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    eprintln!(
                        "tightbeam: failed to create socket directory {}: {e}",
                        parent.display()
                    );
                    std::process::exit(1);
                }
            }

            let listener = bind_agent_socket(socket_path).unwrap_or_else(|e| {
                eprintln!("tightbeam: failed to bind socket at {SOCKET_PATH}: {e}");
                std::process::exit(1);
            });

            tracing::info!("bound socket {SOCKET_PATH} (mode 0600)");

            let provider: Provider = Arc::from(agent_config.llm.provider.build());
            let profile: Profile = Arc::new(agent_config);
            let conversation: Conversation = Arc::new(RwLock::new(
                tightbeam_daemon::conversation::ConversationLog::new(logs_dir()),
            ));
            let mcp_state: McpState =
                Arc::new(RwLock::new(tightbeam_daemon::mcp::McpManager::new(vec![])));

            // Ensure logs directory exists
            if let Err(e) = std::fs::create_dir_all(logs_dir()) {
                tracing::warn!("failed to create logs directory: {e}");
            }

            // Rebuild conversation from existing logs
            if Path::new(LOGS_PATH).exists() {
                match tightbeam_daemon::conversation::ConversationLog::rebuild(logs_dir()) {
                    Ok(log) => {
                        let msg_count = log.history().len();
                        if msg_count > 0 {
                            tracing::info!("rebuilt {msg_count} messages");
                        }
                        *conversation.write().await = log;
                    }
                    Err(e) => {
                        tracing::warn!("failed to rebuild conversation: {e}");
                    }
                }
            }

            run_daemon(listener, profile, conversation, provider, mcp_state).await;
        }
        _ => {
            eprintln!("usage: tightbeam-daemon <start|logs|send|check|version>");
            std::process::exit(1);
        }
    }
}

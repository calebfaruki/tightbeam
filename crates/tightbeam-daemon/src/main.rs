use tightbeam_daemon::config::AgentConfig;
use tightbeam_daemon::{
    bind_agent_socket, run_daemon, Conversation, McpState, Profile, Provider,
};

use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use tightbeam_protocol::framing::{read_frame, write_frame};
use tokio::net::UnixStream;
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

use tightbeam_protocol::SOCKET_PATH;

fn resolve_flag(args: &[String], flag: &str, env_var: &str) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
        i += 1;
    }
    std::env::var(env_var).ok()
}

fn require_flag(args: &[String], flag: &str, env_var: &str) -> Result<String, String> {
    resolve_flag(args, flag, env_var).ok_or_else(|| format!("{flag} is required"))
}

fn require_path(args: &[String], flag: &str, env_var: &str) -> Result<PathBuf, String> {
    require_flag(args, flag, env_var).map(PathBuf::from)
}

fn strip_flags(args: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--no-wait" => {}
            "--file" => {
                i += 1;
            }
            s if s.starts_with("--") => {
                i += 1;
            }
            other => result.push(other.to_string()),
        }
        i += 1;
    }
    result
}

fn mime_from_extension(path: &std::path::Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

async fn send_command(args: &[String], socket_path: &std::path::Path) -> Result<(), String> {
    let mut no_wait = false;
    let mut file_paths: Vec<PathBuf> = Vec::new();
    let mut message_parts: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--no-wait" => no_wait = true,
            "--file" => {
                i += 1;
                if i >= args.len() {
                    return Err("--file requires a path argument".into());
                }
                file_paths.push(PathBuf::from(&args[i]));
            }
            other => message_parts.push(other.to_string()),
        }
        i += 1;
    }

    let message = message_parts.join(" ");
    if message.is_empty() && file_paths.is_empty() {
        return Err("usage: tightbeam-daemon send <message> [--file path] [--no-wait]".into());
    }

    let mut content: Vec<serde_json::Value> = Vec::new();
    if !message.is_empty() {
        content.push(serde_json::json!({"type": "text", "text": message}));
    }

    struct FileInfo {
        path: PathBuf,
        size: u64,
    }
    let mut files: Vec<FileInfo> = Vec::new();

    for path in &file_paths {
        let meta = tokio::fs::metadata(path)
            .await
            .map_err(|e| format!("cannot read '{}': {e}", path.display()))?;
        let size = meta.len();

        if size > u32::MAX as u64 {
            return Err(format!(
                "'{}' is too large ({} bytes, max {})",
                path.display(),
                size,
                u32::MAX
            ));
        }

        let mime = mime_from_extension(path).ok_or_else(|| {
            format!(
                "unsupported file type: '{}' (v1 supports png, jpg, gif, webp)",
                path.display()
            )
        })?;

        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();

        content.push(serde_json::json!({
            "type": "file_incoming",
            "filename": filename,
            "mime_type": mime,
            "size": size
        }));

        files.push(FileInfo {
            path: path.clone(),
            size,
        });
    }

    let stream = UnixStream::connect(socket_path)
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

    if no_wait && files.is_empty() {
        return Ok(());
    }

    for file_info in &files {
        use tightbeam_protocol::framing::write_frame_header;
        use tokio::io::AsyncReadExt;

        let mut file = tokio::fs::File::open(&file_info.path)
            .await
            .map_err(|e| format!("cannot open '{}': {e}", file_info.path.display()))?;

        write_frame_header(&mut writer, file_info.size as u32)
            .await
            .map_err(|e| format!("write error: {e}"))?;

        let mut buf = [0u8; 8192];
        loop {
            let n = file
                .read(&mut buf)
                .await
                .map_err(|e| format!("read error: {e}"))?;
            if n == 0 {
                break;
            }
            tokio::io::AsyncWriteExt::write_all(&mut writer, &buf[..n])
                .await
                .map_err(|e| format!("write error: {e}"))?;
        }
    }
    if !files.is_empty() {
        tokio::io::AsyncWriteExt::flush(&mut writer)
            .await
            .map_err(|e| format!("flush error: {e}"))?;
    }

    if no_wait {
        return Ok(());
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
        "show" => {
            let config_path = require_path(&args, "--config", "TIGHTBEAM_CONFIG")
                .unwrap_or_else(|e| { eprintln!("tightbeam: {e}"); std::process::exit(1); });
            match std::fs::read_to_string(&config_path) {
                Ok(content) => print!("{content}"),
                Err(e) => {
                    eprintln!("tightbeam: failed to read config: {e}");
                    std::process::exit(1);
                }
            }
        }
        "logs" => {
            let name = require_flag(&args, "--name", "TIGHTBEAM_NAME")
                .unwrap_or_else(|e| { eprintln!("tightbeam: {e}"); std::process::exit(1); });
            let logs_dir = require_path(&args, "--logs-dir", "TIGHTBEAM_LOGS_DIR")
                .unwrap_or_else(|e| { eprintln!("tightbeam: {e}"); std::process::exit(1); });
            let log_path = logs_dir.join(&name).join("conversation.ndjson");
            match std::fs::read_to_string(&log_path) {
                Ok(content) => print!("{content}"),
                Err(e) => {
                    eprintln!("tightbeam: failed to read logs: {e}");
                    std::process::exit(1);
                }
            }
        }
        "send" => {
            let send_args = strip_flags(&args[2..]);
            if let Err(e) = send_command(&send_args, std::path::Path::new(SOCKET_PATH)).await {
                eprintln!("tightbeam: {e}");
                std::process::exit(1);
            }
        }
        "check" => {
            let config_path = require_path(&args, "--config", "TIGHTBEAM_CONFIG")
                .unwrap_or_else(|e| { eprintln!("tightbeam: {e}"); std::process::exit(1); });
            match AgentConfig::load(&config_path) {
                Ok(_) => println!("  ok   {}", config_path.display()),
                Err(e) => {
                    eprintln!("  FAIL {e}");
                    std::process::exit(1);
                }
            }
        }
        "start" => {
            let name = require_flag(&args, "--name", "TIGHTBEAM_NAME")
                .unwrap_or_else(|e| { eprintln!("tightbeam: {e}"); std::process::exit(1); });
            let config_path = require_path(&args, "--config", "TIGHTBEAM_CONFIG")
                .unwrap_or_else(|e| { eprintln!("tightbeam: {e}"); std::process::exit(1); });
            let socket_path = PathBuf::from(SOCKET_PATH);
            let logs_dir = require_path(&args, "--logs-dir", "TIGHTBEAM_LOGS_DIR")
                .unwrap_or_else(|e| { eprintln!("tightbeam: {e}"); std::process::exit(1); });

            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::from_default_env().add_directive("info".parse().unwrap()),
                )
                .init();

            let agent_config = AgentConfig::load(&config_path).unwrap_or_else(|e| {
                eprintln!("tightbeam: {e}");
                std::process::exit(1);
            });

            tracing::info!("loaded config for agent '{name}'");

            if let Some(parent) = socket_path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    eprintln!(
                        "tightbeam: failed to create socket directory {}: {e}",
                        parent.display()
                    );
                    std::process::exit(1);
                }
            }

            let listener = bind_agent_socket(&socket_path).unwrap_or_else(|e| {
                eprintln!(
                    "tightbeam: failed to bind socket at {}: {e}",
                    socket_path.display()
                );
                std::process::exit(1);
            });

            tracing::info!("bound socket {} (mode 0600)", socket_path.display());

            let provider: Provider = Arc::from(agent_config.llm.provider.build());

            let profile: Profile = Arc::new(agent_config);
            let conversation: Conversation = Arc::new(RwLock::new(
                tightbeam_daemon::conversation::ConversationLog::new(&logs_dir.join(&name)),
            ));
            let mcp_state: McpState =
                Arc::new(RwLock::new(tightbeam_daemon::mcp::McpManager::new(vec![])));

            {
                let log_path = logs_dir.join(&name).join("conversation.ndjson");
                if log_path.exists() {
                    match tightbeam_daemon::conversation::ConversationLog::rebuild(
                        &logs_dir.join(&name),
                    ) {
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
            }

            run_daemon(listener, profile, conversation, provider, mcp_state, logs_dir, name)
                .await;
        }
        _ => {
            eprintln!("usage: tightbeam-daemon <start|show|logs|send|check|version>");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(flags: &[&str]) -> Vec<String> {
        flags.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn require_flag_present() {
        let a = args(&["--name", "test-agent"]);
        assert_eq!(require_flag(&a, "--name", "X").unwrap(), "test-agent");
    }

    #[test]
    fn require_flag_missing() {
        let err = require_flag(&[], "--name", "NONEXISTENT_ENV_VAR_XYZ").unwrap_err();
        assert!(err.contains("--name"), "{err}");
    }

    #[test]
    fn strip_flags_extracts_message() {
        let a = args(&["hello", "world", "--no-wait"]);
        let msg = strip_flags(&a);
        assert_eq!(msg, vec!["hello", "world"]);
    }

    #[test]
    fn strip_flags_handles_file() {
        let a = args(&["hello", "--file", "photo.png", "world"]);
        let msg = strip_flags(&a);
        assert_eq!(msg, vec!["hello", "world"]);
    }

    #[test]
    fn strip_flags_empty_message() {
        let a: Vec<String> = args(&["--no-wait"]);
        let msg = strip_flags(&a);
        assert!(msg.is_empty());
    }
}

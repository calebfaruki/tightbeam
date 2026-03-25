use tightbeam_daemon::paths::DaemonPaths;
use tightbeam_daemon::registration;
use tightbeam_daemon::{
    bind_agent_socket, build_providers, run_daemon, ConversationMap, McpManagerMap, ProfileMap,
    ProviderMap,
};

use std::collections::HashMap;
use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tightbeam_protocol::framing::{read_frame, write_frame};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

#[derive(Debug)]
struct ParsedPaths {
    daemon: DaemonPaths,
    agents_dir: PathBuf,
}

fn resolve_flag_or_env(
    args: &[String],
    flag: &str,
    env_var: &str,
    env_resolver: &dyn Fn(&str) -> Option<String>,
) -> Option<PathBuf> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag && i + 1 < args.len() {
            return Some(PathBuf::from(&args[i + 1]));
        }
        i += 1;
    }
    env_resolver(env_var).map(PathBuf::from)
}

fn build_daemon_paths(
    args: &[String],
    env_resolver: &dyn Fn(&str) -> Option<String>,
) -> Result<ParsedPaths, String> {
    let config_dir =
        resolve_flag_or_env(args, "--config-dir", "TIGHTBEAM_CONFIG_DIR", env_resolver)
            .ok_or("--config-dir is required")?;
    let sockets_dir =
        resolve_flag_or_env(args, "--sockets-dir", "TIGHTBEAM_SOCKETS_DIR", env_resolver)
            .ok_or("--sockets-dir is required")?;
    let logs_dir = resolve_flag_or_env(args, "--logs-dir", "TIGHTBEAM_LOGS_DIR", env_resolver)
        .ok_or("--logs-dir is required")?;

    let daemon = DaemonPaths {
        config_dir,
        sockets_dir,
        logs_dir,
    };

    let agents_dir =
        resolve_flag_or_env(args, "--agents-dir", "TIGHTBEAM_AGENTS_DIR", env_resolver)
            .unwrap_or_else(|| daemon.config_dir.join("agents"));

    Ok(ParsedPaths { daemon, agents_dir })
}

fn parse_path_flags(args: &[String]) -> ParsedPaths {
    build_daemon_paths(args, &|name| std::env::var(name).ok()).unwrap_or_else(|e| {
        eprintln!("tightbeam: {e}");
        std::process::exit(1);
    })
}

fn show_command(args: &[String], agents_dir: &Path) {
    let agent = match args.first() {
        Some(a) if !a.starts_with("--") => a,
        _ => {
            eprintln!("usage: tightbeam-daemon show <agent>");
            std::process::exit(1);
        }
    };

    let profile_path = agents_dir.join(agent).join("tightbeam.toml");
    match std::fs::read_to_string(&profile_path) {
        Ok(content) => print!("{content}"),
        Err(e) => {
            eprintln!("tightbeam: agent '{agent}' not found: {e}");
            std::process::exit(1);
        }
    }
}

fn logs_command(args: &[String], paths: &DaemonPaths) {
    let agent = match args.first() {
        Some(a) if !a.starts_with("--") => a,
        _ => {
            eprintln!("usage: tightbeam-daemon logs <agent>");
            std::process::exit(1);
        }
    };

    let log_path = paths.logs_dir.join(agent).join("conversation.ndjson");
    match std::fs::read_to_string(&log_path) {
        Ok(content) => print!("{content}"),
        Err(e) => {
            eprintln!("failed to read logs for '{agent}': {e}");
            std::process::exit(1);
        }
    }
}

fn status_command(paths: &DaemonPaths, agents_dir: &Path) {
    if !agents_dir.exists() {
        println!("tightbeam: no agents directory");
        return;
    }

    let mut agents: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(agents_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join("tightbeam.toml").exists() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    agents.push(name.to_string());
                }
            }
        }
    }
    agents.sort();

    if agents.is_empty() {
        println!("tightbeam: no agents found");
        return;
    }

    println!("tightbeam: {} agent(s):", agents.len());
    for agent in &agents {
        let sock_path = paths.sockets_dir.join(format!("{agent}.sock"));
        let status = if sock_path.exists() {
            "active"
        } else {
            "no socket"
        };
        println!("  {agent}\t{status}");
    }
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

async fn send_command(
    args: &[String],
    paths: &DaemonPaths,
    agents_dir: &Path,
) -> Result<(), String> {
    if args.is_empty() {
        return Err(
            "usage: tightbeam-daemon send <agent> <message> [--file path] [--no-wait]".into(),
        );
    }

    let agent = match args.first() {
        Some(a) if !a.starts_with("--") => a,
        _ => {
            return Err(
                "usage: tightbeam-daemon send <agent> <message> [--file path] [--no-wait]".into(),
            );
        }
    };

    let mut no_wait = false;
    let mut file_paths: Vec<PathBuf> = Vec::new();
    let mut message_parts: Vec<String> = Vec::new();

    let mut i = 1;
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
            s if s.starts_with("--") => {}
            other => message_parts.push(other.to_string()),
        }
        i += 1;
    }

    let message = message_parts.join(" ");
    if message.is_empty() && file_paths.is_empty() {
        return Err(
            "usage: tightbeam-daemon send <agent> <message> [--file path] [--no-wait]".into(),
        );
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

    if !agents_dir
        .join(agent.as_str())
        .join("tightbeam.toml")
        .exists()
    {
        return Err(format!(
            "agent '{agent}' not found in {}",
            agents_dir.display()
        ));
    }

    let sock_path = paths.sockets_dir.join(format!("{agent}.sock"));
    let stream = UnixStream::connect(&sock_path)
        .await
        .map_err(|e| format!("cannot connect to agent '{agent}': {e}"))?;
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

fn check_command(parsed: &ParsedPaths) {
    if !parsed.agents_dir.exists() {
        println!(
            "tightbeam: no agents directory at {}",
            parsed.agents_dir.display()
        );
        return;
    }

    let mut agents: Vec<(String, std::path::PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&parsed.agents_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join("tightbeam.toml").exists() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    agents.push((name.to_string(), path));
                }
            }
        }
    }
    agents.sort_by(|a, b| a.0.cmp(&b.0));

    if agents.is_empty() {
        println!("tightbeam: no agents found");
        return;
    }

    let mut failures = 0;
    for (name, path) in &agents {
        let agent = registration::AgentPaths::new(path.clone());
        match agent.load_profile() {
            Ok(_) => println!("  ok   {name}"),
            Err(e) => {
                eprintln!("  FAIL {name}: {e}");
                failures += 1;
            }
        }
    }

    if failures > 0 {
        eprintln!("tightbeam: {failures} agent(s) failed validation");
        std::process::exit(1);
    }
    println!("tightbeam: all agents valid");
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let subcommand = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    if subcommand == "version" {
        println!("tightbeam-daemon {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let parsed = parse_path_flags(&args);

    match subcommand {
        "start" => {}
        "status" => {
            status_command(&parsed.daemon, &parsed.agents_dir);
            return;
        }
        "show" => {
            show_command(&args[2..], &parsed.agents_dir);
            return;
        }
        "logs" => {
            logs_command(&args[2..], &parsed.daemon);
            return;
        }
        "send" => {
            if let Err(e) = send_command(&args[2..], &parsed.daemon, &parsed.agents_dir).await {
                eprintln!("tightbeam: {e}");
                std::process::exit(1);
            }
            return;
        }
        "check" => {
            check_command(&parsed);
            return;
        }
        _ => {
            eprintln!("usage: tightbeam-daemon <start|status|show|logs|send|check|version>");
            std::process::exit(1);
        }
    }

    // --- start command ---

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let DaemonPaths {
        config_dir: _,
        sockets_dir,
        logs_dir,
    } = parsed.daemon;

    let profile_map = registration::load_agents(&parsed.agents_dir).unwrap_or_else(|e| {
        eprintln!("tightbeam: {e}");
        std::process::exit(1);
    });

    if profile_map.is_empty() {
        tracing::warn!(
            "no agents registered — add entries to {}",
            parsed.agents_dir.display()
        );
    }

    if let Err(e) = std::fs::create_dir_all(&sockets_dir) {
        eprintln!(
            "tightbeam: failed to create sockets directory {}: {e}",
            sockets_dir.display()
        );
        std::process::exit(1);
    }

    let mut listeners: Vec<(String, UnixListener)> = Vec::new();
    let mut agent_names: Vec<String> = profile_map.keys().cloned().collect();
    agent_names.sort();

    for name in &agent_names {
        let sock_path = sockets_dir.join(format!("{name}.sock"));
        match bind_agent_socket(&sock_path) {
            Ok(l) => {
                tracing::info!("bound socket {} (mode 0600)", sock_path.display());
                listeners.push((name.to_string(), l));
            }
            Err(e) => {
                eprintln!(
                    "tightbeam: failed to bind socket at {}: {e}",
                    sock_path.display()
                );
                std::process::exit(1);
            }
        }
    }

    if !agent_names.is_empty() {
        tracing::info!(
            "listening on {} agent socket(s): {}",
            listeners.len(),
            agent_names.join(", ")
        );
    }

    let provider_map = build_providers(&profile_map);

    let profiles: ProfileMap = Arc::new(RwLock::new(profile_map));
    let conversations: ConversationMap = Arc::new(RwLock::new(HashMap::new()));
    let providers: ProviderMap = Arc::new(RwLock::new(provider_map));
    let mcp_managers: McpManagerMap = Arc::new(RwLock::new(HashMap::new()));

    {
        let mut convos = conversations.write().await;
        for name in &agent_names {
            let agent_log_dir = logs_dir.join(name);
            if agent_log_dir.join("conversation.ndjson").exists() {
                match tightbeam_daemon::conversation::ConversationLog::rebuild(&agent_log_dir) {
                    Ok(log) => {
                        let msg_count = log.history().len();
                        if msg_count > 0 {
                            tracing::info!("rebuilt {msg_count} messages for agent {name}");
                        }
                        convos.insert(name.to_string(), log);
                    }
                    Err(e) => {
                        tracing::warn!("failed to rebuild conversation for {name}: {e}");
                    }
                }
            }
        }
    }

    run_daemon(
        listeners,
        profiles,
        conversations,
        providers,
        mcp_managers,
        logs_dir,
        sockets_dir,
        parsed.agents_dir,
    )
    .await;
}

#[cfg(test)]
mod path_flag_tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn args(flags: &[&str]) -> Vec<String> {
        flags.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn all_three_flags_builds_paths() {
        let a = args(&[
            "--config-dir",
            "/etc/tb",
            "--sockets-dir",
            "/run/tb/sockets",
            "--logs-dir",
            "/var/log/tb",
        ]);
        let p = build_daemon_paths(&a, &no_env).unwrap();
        assert_eq!(p.daemon.config_dir, PathBuf::from("/etc/tb"));
        assert_eq!(p.daemon.sockets_dir, PathBuf::from("/run/tb/sockets"));
        assert_eq!(p.daemon.logs_dir, PathBuf::from("/var/log/tb"));
    }

    #[test]
    fn missing_config_dir_is_error() {
        let a = args(&["--sockets-dir", "/run/tb", "--logs-dir", "/var/log/tb"]);
        let err = build_daemon_paths(&a, &no_env).unwrap_err();
        assert!(err.contains("--config-dir"), "{err}");
    }

    #[test]
    fn missing_sockets_dir_is_error() {
        let a = args(&["--config-dir", "/etc/tb", "--logs-dir", "/var/log/tb"]);
        let err = build_daemon_paths(&a, &no_env).unwrap_err();
        assert!(err.contains("--sockets-dir"), "{err}");
    }

    #[test]
    fn missing_logs_dir_is_error() {
        let a = args(&["--config-dir", "/etc/tb", "--sockets-dir", "/run/tb"]);
        let err = build_daemon_paths(&a, &no_env).unwrap_err();
        assert!(err.contains("--logs-dir"), "{err}");
    }

    #[test]
    fn no_flags_is_error() {
        let err = build_daemon_paths(&[], &no_env).unwrap_err();
        assert!(err.contains("required"), "{err}");
    }

    #[test]
    fn agents_defaults_to_config_dir() {
        let a = args(&[
            "--config-dir",
            "/etc/tb",
            "--sockets-dir",
            "/run/tb/sockets",
            "--logs-dir",
            "/var/log/tb",
        ]);
        let p = build_daemon_paths(&a, &no_env).unwrap();
        assert_eq!(p.agents_dir, PathBuf::from("/etc/tb/agents"));
    }

    #[test]
    fn agents_flag_overrides_default() {
        let a = args(&[
            "--config-dir",
            "/etc/tb",
            "--sockets-dir",
            "/run/tb/sockets",
            "--logs-dir",
            "/var/log/tb",
            "--agents-dir",
            "/custom/agents",
        ]);
        let p = build_daemon_paths(&a, &no_env).unwrap();
        assert_eq!(p.agents_dir, PathBuf::from("/custom/agents"));
    }

    #[test]
    fn cli_flag_overrides_env_var() {
        let env = |name: &str| match name {
            "TIGHTBEAM_CONFIG_DIR" => Some("/from-env".into()),
            "TIGHTBEAM_SOCKETS_DIR" => Some("/from-env/sockets".into()),
            "TIGHTBEAM_LOGS_DIR" => Some("/from-env/logs".into()),
            _ => None,
        };
        let a = args(&[
            "--config-dir",
            "/from-flag",
            "--sockets-dir",
            "/from-flag/sockets",
            "--logs-dir",
            "/from-flag/logs",
        ]);
        let p = build_daemon_paths(&a, &env).unwrap();
        assert_eq!(p.daemon.config_dir, PathBuf::from("/from-flag"));
    }

    #[test]
    fn env_var_used_when_no_flag() {
        let env = |name: &str| match name {
            "TIGHTBEAM_CONFIG_DIR" => Some("/from-env".into()),
            "TIGHTBEAM_SOCKETS_DIR" => Some("/from-env/sockets".into()),
            "TIGHTBEAM_LOGS_DIR" => Some("/from-env/logs".into()),
            _ => None,
        };
        let p = build_daemon_paths(&[], &env).unwrap();
        assert_eq!(p.daemon.config_dir, PathBuf::from("/from-env"));
    }
}

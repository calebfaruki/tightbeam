use tightbeam_daemon::profile;
use tightbeam_daemon::provider::claude::ClaudeProvider;
use tightbeam_daemon::provider::LlmProvider;
use tightbeam_daemon::{
    bind_agent_socket, run_daemon, ConversationMap, McpManagerMap, ProfileMap, ProviderMap,
};

use std::collections::HashMap;
use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

extern crate libc;
use tightbeam_protocol::framing::{read_frame, write_frame};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

fn config_dir() -> PathBuf {
    let home = env::var("HOME").expect("HOME not set");
    PathBuf::from(home).join(".config").join("tightbeam")
}

fn agents_dir() -> PathBuf {
    config_dir().join("agents")
}

fn sockets_dir() -> PathBuf {
    config_dir().join("sockets")
}

fn logs_dir() -> PathBuf {
    let home = env::var("HOME").expect("HOME not set");
    PathBuf::from(home)
        .join(".local/share/tightbeam")
        .join("logs")
}

fn pid_path() -> PathBuf {
    config_dir().join("tightbeam.pid")
}

fn write_pid_file() {
    let pid = std::process::id();
    if let Err(e) = std::fs::write(pid_path(), pid.to_string()) {
        tracing::warn!("failed to write pid file: {e}");
    }
}

fn remove_pid_file() {
    let _ = std::fs::remove_file(pid_path());
}

fn restart_command() {
    let path = pid_path();
    let pid_str = match std::fs::read_to_string(&path) {
        Ok(s) => s.trim().to_string(),
        Err(_) => {
            eprintln!("tightbeam: no pid file found — is the daemon running?");
            std::process::exit(1);
        }
    };

    let pid: i32 = match pid_str.parse() {
        Ok(p) => p,
        Err(_) => {
            eprintln!("tightbeam: invalid pid file");
            let _ = std::fs::remove_file(&path);
            std::process::exit(1);
        }
    };

    // Check if process is alive
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    if !alive {
        eprintln!("tightbeam: stale pid file (process {pid} not running)");
        let _ = std::fs::remove_file(&path);
        std::process::exit(1);
    }

    // Send SIGHUP
    let result = unsafe { libc::kill(pid, libc::SIGHUP) };
    if result != 0 {
        eprintln!("tightbeam: failed to send SIGHUP to process {pid}");
        std::process::exit(1);
    }

    println!("tightbeam: config reload triggered (pid {pid})");
}

fn show_command(args: &[String]) {
    let agent = match args.first() {
        Some(a) => a,
        None => {
            eprintln!("usage: tightbeam-daemon show <agent>");
            std::process::exit(1);
        }
    };

    let path = agents_dir().join(format!("{agent}.toml"));
    match std::fs::read_to_string(&path) {
        Ok(content) => print!("{content}"),
        Err(e) => {
            eprintln!("failed to read agent profile '{agent}': {e}");
            std::process::exit(1);
        }
    }
}

fn logs_command(args: &[String]) {
    let agent = match args.first() {
        Some(a) => a,
        None => {
            eprintln!("usage: tightbeam-daemon logs <agent>");
            std::process::exit(1);
        }
    };

    let log_path = logs_dir().join(agent).join("conversation.ndjson");
    match std::fs::read_to_string(&log_path) {
        Ok(content) => print!("{content}"),
        Err(e) => {
            eprintln!("failed to read logs for '{agent}': {e}");
            std::process::exit(1);
        }
    }
}

fn status_command() {
    let sock_dir = sockets_dir();
    if !sock_dir.exists() {
        println!("tightbeam: not initialized (run 'tightbeam-daemon init')");
        return;
    }

    let mut agents: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&sock_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("sock") {
                if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                    agents.push(name.to_string());
                }
            }
        }
    }
    agents.sort();

    if agents.is_empty() {
        println!("tightbeam: no active agent sockets");
    } else {
        println!("tightbeam: {} agent socket(s):", agents.len());
        for agent in &agents {
            println!(
                "  {agent}\t{}",
                sock_dir.join(format!("{agent}.sock")).display()
            );
        }
    }
}

async fn send_command(args: &[String]) -> Result<(), String> {
    if args.len() < 2 {
        return Err("usage: tightbeam-daemon send <agent> <message>".into());
    }

    let agent = &args[0];
    let no_wait = args.iter().any(|a| a == "--no-wait");
    let message: String = args[1..]
        .iter()
        .filter(|a| a.as_str() != "--no-wait")
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");

    if message.is_empty() {
        return Err("usage: tightbeam-daemon send <agent> <message>".into());
    }

    let sock_path = sockets_dir().join(format!("{agent}.sock"));
    let stream = UnixStream::connect(&sock_path)
        .await
        .map_err(|e| format!("cannot connect to agent '{agent}': {e}"))?;
    let (mut reader, mut writer) = stream.into_split();

    let rpc = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "send",
        "params": {"content": [{"type": "text", "text": message}]}
    });
    let payload = serde_json::to_vec(&rpc).map_err(|e| format!("serialize error: {e}"))?;
    write_frame(&mut writer, &payload)
        .await
        .map_err(|e| format!("write error: {e}"))?;

    loop {
        let frame = read_frame(&mut reader)
            .await
            .map_err(|e| format!("read error: {e}"))?
            .ok_or("daemon disconnected")?;
        let raw = String::from_utf8_lossy(&frame);
        let value: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| format!("invalid JSON: {e}"))?;

        if value.get("id").is_some() {
            if let Some(error) = value.get("error") {
                let msg = error["message"].as_str().unwrap_or("unknown error");
                return Err(msg.to_string());
            }
            if no_wait {
                return Ok(());
            }
        } else if let Some(method) = value.get("method").and_then(|m| m.as_str()) {
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
        "start" => {}
        "version" => {
            println!("tightbeam-daemon {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        "init" => {
            let config = tightbeam_daemon::init::InitConfig::detect();
            let uninstall = args.iter().any(|a| a == "--uninstall");
            if uninstall {
                tightbeam_daemon::init::run_uninstall(&config).unwrap_or_else(|e| {
                    eprintln!("tightbeam: {e}");
                    std::process::exit(1);
                });
            } else {
                tightbeam_daemon::init::run_init(&config).unwrap_or_else(|e| {
                    eprintln!("tightbeam: {e}");
                    std::process::exit(1);
                });
            }
            return;
        }
        "status" => {
            status_command();
            return;
        }
        "show" => {
            show_command(&args[2..]);
            return;
        }
        "logs" => {
            logs_command(&args[2..]);
            return;
        }
        "restart" => {
            restart_command();
            return;
        }
        "stop" => {
            let config = tightbeam_daemon::init::InitConfig::detect();
            tightbeam_daemon::init::run_uninstall(&config).unwrap_or_else(|e| {
                eprintln!("tightbeam: {e}");
                std::process::exit(1);
            });
            return;
        }
        "send" => {
            if let Err(e) = send_command(&args[2..]).await {
                eprintln!("tightbeam: {e}");
                std::process::exit(1);
            }
            return;
        }
        _ => {
            eprintln!(
                "usage: tightbeam-daemon <start|restart|stop|init|status|show|logs|send|version>"
            );
            std::process::exit(1);
        }
    }

    // --- start command ---

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let cfg_dir = config_dir();
    let (_registry, profile_map) = profile::load_all(&cfg_dir).unwrap_or_else(|e| {
        eprintln!("tightbeam: {e}");
        std::process::exit(1);
    });

    if profile_map.is_empty() {
        eprintln!(
            "tightbeam: no agent profiles found in {} — create at least one to start",
            agents_dir().display()
        );
        std::process::exit(1);
    }

    // Bind sockets
    let sock_dir = sockets_dir();
    if let Err(e) = std::fs::create_dir_all(&sock_dir) {
        eprintln!(
            "tightbeam: failed to create sockets directory {}: {e}",
            sock_dir.display()
        );
        std::process::exit(1);
    }

    let mut listeners: Vec<(String, UnixListener)> = Vec::new();
    let mut agent_names: Vec<String> = profile_map.keys().cloned().collect();
    agent_names.sort();

    for name in &agent_names {
        let sock_path = sock_dir.join(format!("{name}.sock"));
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

    tracing::info!(
        "listening on {} agent socket(s): {}",
        listeners.len(),
        agent_names.join(", ")
    );

    // Initialize providers
    let mut provider_map: HashMap<String, Box<dyn LlmProvider>> = HashMap::new();
    provider_map.insert("anthropic".into(), Box::new(ClaudeProvider::new()));

    let profiles: ProfileMap = Arc::new(RwLock::new(profile_map));
    let conversations: ConversationMap = Arc::new(RwLock::new(HashMap::new()));
    let providers: ProviderMap = Arc::new(provider_map);
    let mcp_managers: McpManagerMap = Arc::new(RwLock::new(HashMap::new()));

    write_pid_file();

    // Rebuild conversations from existing logs
    let log_base = logs_dir();
    {
        let mut convos = conversations.write().await;
        for name in &agent_names {
            let agent_log_dir = log_base.join(name);
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
        log_base,
        cfg_dir,
    )
    .await;

    remove_pid_file();
}

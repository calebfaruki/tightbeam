use tightbeam_daemon::lifecycle;
use tightbeam_daemon::profile::AgentProfile;
use tightbeam_daemon::provider::claude::ClaudeProvider;
use tightbeam_daemon::provider::LlmProvider;
use tightbeam_daemon::{bind_agent_socket, run_daemon, ConversationMap, ProfileMap, ProviderMap};

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixListener;
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
        "stop" => {
            let config = tightbeam_daemon::init::InitConfig::detect();
            tightbeam_daemon::init::run_uninstall(&config).unwrap_or_else(|e| {
                eprintln!("tightbeam: {e}");
                std::process::exit(1);
            });
            return;
        }
        _ => {
            eprintln!("usage: tightbeam-daemon <start|stop|init|status|show|logs|version>");
            std::process::exit(1);
        }
    }

    // --- start command ---

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let agent_dir = agents_dir();
    let mut profile_map: HashMap<String, AgentProfile> = HashMap::new();

    if agent_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&agent_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                    continue;
                }
                let name = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };
                match AgentProfile::load(&path) {
                    Ok(profile) => {
                        profile_map.insert(name, profile);
                    }
                    Err(e) => {
                        eprintln!(
                            "tightbeam: failed to load profile '{}': {e}",
                            path.display()
                        );
                        std::process::exit(1);
                    }
                }
            }
        }
    }

    if profile_map.is_empty() {
        eprintln!(
            "tightbeam: no agent profiles found in {} — create at least one to start",
            agent_dir.display()
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
    provider_map.insert("claude".into(), Box::new(ClaudeProvider::new()));

    let profiles: ProfileMap = Arc::new(profile_map);
    let conversations: ConversationMap = Arc::new(RwLock::new(HashMap::new()));
    let providers: ProviderMap = Arc::new(provider_map);
    let idle_map = lifecycle::new_idle_map();

    // Initialize idle timestamps for all agents
    for name in &agent_names {
        lifecycle::touch(&idle_map, name).await;
    }

    // Spawn idle checker (30 minute timeout)
    let idle_clone = idle_map.clone();
    tokio::spawn(lifecycle::run_idle_check(
        idle_clone,
        Duration::from_secs(1800),
    ));

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
        idle_map,
        log_base,
    )
    .await;
}

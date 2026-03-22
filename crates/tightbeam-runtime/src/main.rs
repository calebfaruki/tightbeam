mod agent;
mod config;
mod connection;
mod tools;

use config::RuntimeConfig;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let config = match RuntimeConfig::from_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tightbeam-runtime: {e}");
            eprintln!("usage: tightbeam --system-prompt <path> --tools <list> --socket <path> [--max-iterations <n>] [--max-output-chars <n>]");
            std::process::exit(1);
        }
    };

    if let Err(e) = agent::run_agent(config).await {
        eprintln!("tightbeam-runtime: {e}");
        std::process::exit(1);
    }
}

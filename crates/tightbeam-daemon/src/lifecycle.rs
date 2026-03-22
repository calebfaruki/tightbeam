use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

pub type IdleMap = Arc<RwLock<HashMap<String, Instant>>>;

pub fn new_idle_map() -> IdleMap {
    Arc::new(RwLock::new(HashMap::new()))
}

pub async fn touch(idle_map: &IdleMap, agent: &str) {
    let mut map = idle_map.write().await;
    map.insert(agent.to_string(), Instant::now());
}

pub async fn idle_agents(idle_map: &IdleMap, timeout: Duration) -> Vec<String> {
    let map = idle_map.read().await;
    let now = Instant::now();
    map.iter()
        .filter(|(_, last)| now.duration_since(**last) > timeout)
        .map(|(name, _)| name.clone())
        .collect()
}

pub async fn stop_agent(agent: &str) -> Result<(), String> {
    let service_name = format!("medina-agent-{agent}.service");
    let status = tokio::process::Command::new("systemctl")
        .args(["--user", "stop", &service_name])
        .status()
        .await
        .map_err(|e| format!("failed to run systemctl stop: {e}"))?;

    if status.success() {
        tracing::info!("stopped idle agent {agent}");
        Ok(())
    } else {
        Err(format!("systemctl stop {service_name} failed"))
    }
}

pub async fn run_idle_check(idle_map: IdleMap, timeout: Duration) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;
        let stale = idle_agents(&idle_map, timeout).await;
        for agent in stale {
            if let Err(e) = stop_agent(&agent).await {
                tracing::warn!("failed to stop idle agent {agent}: {e}");
            }
            let mut map = idle_map.write().await;
            map.remove(&agent);
        }
    }
}

#[cfg(test)]
mod idle_tracking {
    use super::*;

    #[tokio::test]
    async fn touch_updates_timestamp() {
        let map = new_idle_map();
        touch(&map, "test-agent").await;

        let inner = map.read().await;
        assert!(inner.contains_key("test-agent"));
    }

    #[tokio::test]
    async fn idle_agents_returns_stale_entries() {
        let map = new_idle_map();

        {
            let mut inner = map.write().await;
            inner.insert(
                "stale-agent".into(),
                Instant::now() - Duration::from_secs(3600),
            );
            inner.insert("fresh-agent".into(), Instant::now());
        }

        let stale = idle_agents(&map, Duration::from_secs(600)).await;
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0], "stale-agent");
    }

    #[tokio::test]
    async fn idle_agents_returns_empty_when_all_fresh() {
        let map = new_idle_map();
        touch(&map, "agent-a").await;
        touch(&map, "agent-b").await;

        let stale = idle_agents(&map, Duration::from_secs(600)).await;
        assert!(stale.is_empty());
    }
}

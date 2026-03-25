use crate::profile::AgentConfig;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct AgentPaths {
    pub root: PathBuf,
}

impl AgentPaths {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn profile_path(&self) -> PathBuf {
        self.root.join("tightbeam.toml")
    }

    pub fn load_profile(&self) -> Result<AgentConfig, String> {
        AgentConfig::load(&self.profile_path())
    }
}

pub fn load_agents(agents_dir: &Path) -> Result<HashMap<String, AgentConfig>, String> {
    if !agents_dir.exists() {
        return Ok(HashMap::new());
    }
    let mut configs = HashMap::new();
    for entry in std::fs::read_dir(agents_dir).map_err(|e| format!("{e}"))? {
        let entry = entry.map_err(|e| format!("{e}"))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let agent = AgentPaths::new(path);
        if !agent.profile_path().exists() {
            continue;
        }
        let config = agent.load_profile()?;
        configs.insert(name, config);
    }
    Ok(configs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_paths_profile_path() {
        let ap = AgentPaths::new(PathBuf::from("/agents/dev"));
        assert_eq!(
            ap.profile_path(),
            PathBuf::from("/agents/dev/tightbeam.toml")
        );
    }

    #[test]
    fn scan_nonexistent_dir_returns_zero() {
        let configs = load_agents(Path::new("/nonexistent/agents/dir")).unwrap();
        assert!(configs.is_empty());
    }

    #[test]
    fn scan_empty_dir_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        let configs = load_agents(dir.path()).unwrap();
        assert!(configs.is_empty());
    }

    #[test]
    fn scan_skips_non_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("not-a-dir.txt"), "hello").unwrap();
        let configs = load_agents(dir.path()).unwrap();
        assert!(configs.is_empty());
    }

    #[test]
    fn scan_skips_dirs_without_tightbeam_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("empty-agent")).unwrap();
        let configs = load_agents(dir.path()).unwrap();
        assert!(configs.is_empty());
    }

    #[test]
    fn scan_dir_with_agents() {
        let dir = tempfile::tempdir().unwrap();

        let secret = dir.path().join("api_key");
        std::fs::write(&secret, "test-key").unwrap();

        let agent_dir = dir.path().join("my-agent");
        std::fs::create_dir(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("tightbeam.toml"),
            format!(
                "[llm]\nprovider = \"anthropic\"\nmodel = \"m\"\napi_key_file = \"{}\"",
                secret.display()
            ),
        )
        .unwrap();

        let configs = load_agents(dir.path()).unwrap();
        assert_eq!(configs.len(), 1);
        assert!(configs.contains_key("my-agent"));
    }
}

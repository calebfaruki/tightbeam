use crate::profile::{AgentProfile, Registry};
use serde::Deserialize;
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

    pub fn load_profile(
        &self,
        registry: &Registry,
        env_resolver: &dyn Fn(&str) -> Option<String>,
    ) -> Result<AgentProfile, String> {
        AgentProfile::load(&self.profile_path(), registry, env_resolver)
    }
}

#[derive(Debug)]
pub struct AgentRegistration {
    pub name: String,
    pub paths: AgentPaths,
}

#[derive(Deserialize)]
struct AgentEntry {
    path: String,
}

pub fn parse_registration(toml_str: &str) -> Result<Vec<AgentRegistration>, String> {
    let table: HashMap<String, AgentEntry> =
        toml::from_str(toml_str).map_err(|e| format!("invalid agents file: {e}"))?;

    let mut registrations: Vec<AgentRegistration> = table
        .into_iter()
        .map(|(name, entry)| AgentRegistration {
            name,
            paths: AgentPaths::new(PathBuf::from(entry.path)),
        })
        .collect();

    registrations.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(registrations)
}

pub(crate) fn validate_registration(registrations: &[AgentRegistration]) -> Result<(), String> {
    for reg in registrations {
        let root = &reg.paths.root;
        if !root.is_dir() {
            return Err(format!(
                "agent '{}': path '{}' is not a directory",
                reg.name,
                root.display()
            ));
        }
        if !reg.paths.profile_path().exists() {
            return Err(format!(
                "agent '{}': missing tightbeam.toml in '{}'",
                reg.name,
                root.display()
            ));
        }
    }
    Ok(())
}

pub fn load_agents(
    agents_path: &Path,
    registry_path: &Path,
    env_resolver: &dyn Fn(&str) -> Option<String>,
) -> Result<(Registry, HashMap<String, AgentProfile>), String> {
    let registry = if registry_path.exists() {
        Registry::load(registry_path)?
    } else {
        Registry::empty()
    };
    if !agents_path.exists() {
        return Ok((registry, HashMap::new()));
    }
    let registrations = load_registration(agents_path)?;
    let mut profiles = HashMap::new();
    for reg in registrations {
        let profile = reg.paths.load_profile(&registry, env_resolver)?;
        profiles.insert(reg.name, profile);
    }
    Ok((registry, profiles))
}

pub fn load_registration(path: &Path) -> Result<Vec<AgentRegistration>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read agents file {}: {e}", path.display()))?;
    let registrations = parse_registration(&content)?;
    validate_registration(&registrations)?;
    Ok(registrations)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Pure parsing tests (no filesystem) ---

    #[test]
    fn parse_empty_file_returns_empty_vec() {
        let result = parse_registration("").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn parse_valid_entries() {
        let toml = r#"
[dev-agent]
path = "/home/caleb/agents/dev-agent"

[deploy-agent]
path = "/home/caleb/agents/deploy-agent"
"#;
        let result = parse_registration(toml).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, "deploy-agent");
        assert_eq!(result[1].name, "dev-agent");
    }

    #[test]
    fn parse_invalid_toml_is_error() {
        let err = parse_registration("not valid toml [[[").unwrap_err();
        assert!(err.contains("invalid agents file"), "{err}");
    }

    #[test]
    fn agent_paths_profile_path() {
        let ap = AgentPaths::new(PathBuf::from("/agents/dev"));
        assert_eq!(
            ap.profile_path(),
            PathBuf::from("/agents/dev/tightbeam.toml")
        );
    }

    #[test]
    fn registrations_sorted_by_name() {
        let toml = r#"
[z-agent]
path = "/z"
[a-agent]
path = "/a"
[m-agent]
path = "/m"
"#;
        let result = parse_registration(toml).unwrap();
        let names: Vec<&str> = result.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["a-agent", "m-agent", "z-agent"]);
    }

    // --- Filesystem validation tests ---

    fn temp_dir(suffix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tightbeam-reg-test-{}-{}",
            suffix,
            std::process::id()
        ))
    }

    #[test]
    fn validate_missing_directory_is_error() {
        let regs = vec![AgentRegistration {
            name: "ghost".into(),
            paths: AgentPaths::new(PathBuf::from("/nonexistent/path/xyz")),
        }];
        let err = validate_registration(&regs).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
        assert!(err.contains("ghost"), "{err}");
    }

    #[test]
    fn validate_missing_profile_toml_is_error() {
        let dir = temp_dir("no-toml");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let regs = vec![AgentRegistration {
            name: "incomplete".into(),
            paths: AgentPaths::new(dir.clone()),
        }];
        let err = validate_registration(&regs).unwrap_err();
        assert!(err.contains("missing tightbeam.toml"), "{err}");
        assert!(err.contains("incomplete"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_valid_entries_passes() {
        let dir = temp_dir("valid");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tightbeam.toml"), "[llm.test]\n").unwrap();

        let regs = vec![AgentRegistration {
            name: "good".into(),
            paths: AgentPaths::new(dir.clone()),
        }];
        validate_registration(&regs).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- load_agents tests ---

    #[test]
    fn load_agents_missing_file_returns_zero() {
        let nonexistent = PathBuf::from("/tmp/tightbeam-test-nonexistent-agents.toml");
        let registry_path = PathBuf::from("/tmp/tightbeam-test-nonexistent-registry.toml");
        let (_reg, profiles) = load_agents(&nonexistent, &registry_path, &|_| None).unwrap();
        assert!(profiles.is_empty());
    }

    #[test]
    fn load_agents_bad_path_is_error() {
        let base = temp_dir("bad-path");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let agents_toml = base.join("agents.toml");
        std::fs::write(&agents_toml, "[ghost]\npath = \"/nonexistent/agent/dir\"\n").unwrap();

        let registry_path = base.join("registry.toml");
        let err = load_agents(&agents_toml, &registry_path, &|_| None).unwrap_err();
        assert!(err.contains("ghost"), "{err}");
        assert!(err.contains("not a directory"), "{err}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn load_agents_valid_file() {
        let base = temp_dir("valid-load");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        // Create agent dir with tightbeam.toml
        let agent_dir = base.join("my-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("tightbeam.toml"), "[llm.claude]\n").unwrap();

        // Create agents.toml
        let agents_toml = base.join("agents.toml");
        std::fs::write(
            &agents_toml,
            format!("[my-agent]\npath = \"{}\"\n", agent_dir.display()),
        )
        .unwrap();

        // Create registry.toml
        let registry_path = base.join("registry.toml");
        std::fs::write(
            &registry_path,
            r#"
[llm.claude]
provider = "anthropic"
model = "claude-sonnet"
api_key = "test-key"
"#,
        )
        .unwrap();

        let (_reg, profiles) = load_agents(&agents_toml, &registry_path, &|_| None).unwrap();
        assert_eq!(profiles.len(), 1);
        assert!(profiles.contains_key("my-agent"));

        let _ = std::fs::remove_dir_all(&base);
    }
}

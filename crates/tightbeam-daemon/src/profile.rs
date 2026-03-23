use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

// --- Registry (parsed from ~/.config/tightbeam/registry.toml) ---

#[derive(Debug, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub llm: HashMap<String, RegistryLlm>,
    #[serde(default)]
    pub mcp: HashMap<String, RegistryMcp>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegistryLlm {
    pub provider: String,
    pub model: String,
    pub api_key: String,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegistryMcp {
    pub url: String,
    pub auth_token: String,
}

impl Registry {
    pub fn load(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read registry {}: {e}", path.display()))?;
        Self::parse(&content)
    }

    pub fn parse(toml_str: &str) -> Result<Self, String> {
        toml::from_str(toml_str).map_err(|e| format!("invalid registry: {e}"))
    }

    pub fn empty() -> Self {
        Self {
            llm: HashMap::new(),
            mcp: HashMap::new(),
        }
    }
}

// --- Resolved agent profile ---

#[derive(Debug)]
pub struct AgentProfile {
    pub llm: ResolvedLlm,
    pub mcp_servers: Vec<ResolvedMcp>,
}

#[derive(Debug, Clone)]
pub struct ResolvedLlm {
    pub provider: String,
    pub model: String,
    pub api_key: String,
    pub max_tokens: u32,
}

#[derive(Debug, Clone)]
pub struct ResolvedMcp {
    pub name: String,
    pub url: String,
    pub auth_token: String,
    pub tool_allowlist: Option<Vec<String>>,
}

fn default_max_tokens() -> u32 {
    8192
}

// --- Raw TOML structures for agent profiles ---

#[derive(Debug, Deserialize)]
struct RawAgentProfile {
    #[serde(default)]
    llm: HashMap<String, RawLlmOverride>,
    #[serde(default)]
    mcp: HashMap<String, RawMcpOverride>,
}

#[derive(Debug, Deserialize)]
struct RawLlmOverride {
    provider: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RawMcpOverride {
    url: Option<String>,
    auth_token: Option<String>,
    tools: Option<Vec<String>>,
}

impl AgentProfile {
    pub fn resolve(toml_str: &str, registry: &Registry) -> Result<Self, String> {
        let raw: RawAgentProfile =
            toml::from_str(toml_str).map_err(|e| format!("invalid agent profile: {e}"))?;

        if raw.llm.is_empty() {
            return Err("agent profile must have exactly one [llm.*] section".into());
        }
        if raw.llm.len() > 1 {
            return Err(format!(
                "agent profile must have exactly one [llm.*] section, found {}",
                raw.llm.len()
            ));
        }

        let (llm_key, llm_override) = raw.llm.into_iter().next().unwrap();
        let registry_llm = registry
            .llm
            .get(&llm_key)
            .ok_or_else(|| format!("unknown llm registry key: {llm_key}"))?;

        let llm = ResolvedLlm {
            provider: llm_override
                .provider
                .unwrap_or_else(|| registry_llm.provider.clone()),
            model: llm_override
                .model
                .unwrap_or_else(|| registry_llm.model.clone()),
            api_key: llm_override
                .api_key
                .unwrap_or_else(|| registry_llm.api_key.clone()),
            max_tokens: llm_override
                .max_tokens
                .or(registry_llm.max_tokens)
                .unwrap_or_else(default_max_tokens),
        };

        let mut mcp_servers = Vec::new();
        for (mcp_key, mcp_override) in raw.mcp {
            let registry_mcp = registry
                .mcp
                .get(&mcp_key)
                .ok_or_else(|| format!("unknown mcp registry key: {mcp_key}"))?;

            mcp_servers.push(ResolvedMcp {
                name: mcp_key,
                url: mcp_override.url.unwrap_or_else(|| registry_mcp.url.clone()),
                auth_token: mcp_override
                    .auth_token
                    .unwrap_or_else(|| registry_mcp.auth_token.clone()),
                tool_allowlist: mcp_override.tools,
            });
        }

        Ok(AgentProfile { llm, mcp_servers })
    }

    pub fn load(path: &Path, registry: &Registry) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        Self::resolve(&content, registry)
    }
}

pub fn load_all(config_dir: &Path) -> Result<(Registry, HashMap<String, AgentProfile>), String> {
    let registry_path = config_dir.join("registry.toml");
    let registry = if registry_path.exists() {
        Registry::load(&registry_path)?
    } else {
        Registry::empty()
    };

    let agent_dir = config_dir.join("agents");
    let mut profiles = HashMap::new();
    if agent_dir.exists() {
        for entry in std::fs::read_dir(&agent_dir).map_err(|e| format!("{e}"))? {
            let entry = entry.map_err(|e| format!("{e}"))?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let name = match path.file_stem().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            let profile = AgentProfile::load(&path, &registry)?;
            profiles.insert(name, profile);
        }
    }
    Ok((registry, profiles))
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    #[test]
    fn valid_registry_parses() {
        let toml = r#"
[llm.claude-sonnet]
provider = "anthropic"
model = "claude-sonnet-4-20250514"
api_key = "ANTHROPIC_API_KEY"
max_tokens = 8192

[llm.gpt-4o]
provider = "openai"
model = "gpt-4o"
api_key = "OPENAI_API_KEY"

[mcp.github]
url = "https://mcp.github.com/sse"
auth_token = "GITHUB_TOKEN"

[mcp.web-search]
url = "https://mcp.search.example.com/sse"
auth_token = "SEARCH_API_KEY"
"#;
        let reg = Registry::parse(toml).unwrap();
        assert_eq!(reg.llm.len(), 2);
        assert_eq!(reg.mcp.len(), 2);
        assert_eq!(reg.llm["claude-sonnet"].provider, "anthropic");
        assert_eq!(reg.llm["gpt-4o"].api_key, "OPENAI_API_KEY");
        assert_eq!(reg.mcp["github"].url, "https://mcp.github.com/sse");
        assert_eq!(reg.mcp["web-search"].auth_token, "SEARCH_API_KEY");
    }

    #[test]
    fn registry_missing_required_field_fails() {
        let toml = r#"
[llm.claude-sonnet]
provider = "anthropic"
api_key = "ANTHROPIC_API_KEY"
"#;
        let err = Registry::parse(toml).unwrap_err();
        assert!(err.contains("model"), "expected error about model: {err}");
    }

    #[test]
    fn registry_with_only_llm_parses() {
        let toml = r#"
[llm.claude-sonnet]
provider = "anthropic"
model = "claude-sonnet-4-20250514"
api_key = "ANTHROPIC_API_KEY"
"#;
        let reg = Registry::parse(toml).unwrap();
        assert_eq!(reg.llm.len(), 1);
        assert!(reg.mcp.is_empty());
    }

    #[test]
    fn empty_registry_parses() {
        let reg = Registry::parse("").unwrap();
        assert!(reg.llm.is_empty());
        assert!(reg.mcp.is_empty());
    }

    #[test]
    fn registry_max_tokens_optional() {
        let toml = r#"
[llm.claude-sonnet]
provider = "anthropic"
model = "claude-sonnet-4-20250514"
api_key = "ANTHROPIC_API_KEY"
"#;
        let reg = Registry::parse(toml).unwrap();
        assert!(reg.llm["claude-sonnet"].max_tokens.is_none());
    }
}

#[cfg(test)]
mod profile_resolution {
    use super::*;

    fn test_registry() -> Registry {
        Registry::parse(
            r#"
[llm.claude-sonnet]
provider = "anthropic"
model = "claude-sonnet-4-20250514"
api_key = "ANTHROPIC_API_KEY"
max_tokens = 4096

[mcp.github]
url = "https://mcp.github.com/sse"
auth_token = "GITHUB_TOKEN"

[mcp.web-search]
url = "https://mcp.search.example.com/sse"
auth_token = "SEARCH_API_KEY"
"#,
        )
        .unwrap()
    }

    #[test]
    fn empty_llm_body_uses_all_registry_defaults() {
        let reg = test_registry();
        let profile = AgentProfile::resolve("[llm.claude-sonnet]\n", &reg).unwrap();
        assert_eq!(profile.llm.provider, "anthropic");
        assert_eq!(profile.llm.model, "claude-sonnet-4-20250514");
        assert_eq!(profile.llm.api_key, "ANTHROPIC_API_KEY");
        assert_eq!(profile.llm.max_tokens, 4096);
    }

    #[test]
    fn single_field_override_only_overrides_that_field() {
        let reg = test_registry();
        let toml = r#"
[llm.claude-sonnet]
api_key = "AGENT_SPECIFIC_KEY"
"#;
        let profile = AgentProfile::resolve(toml, &reg).unwrap();
        assert_eq!(profile.llm.provider, "anthropic");
        assert_eq!(profile.llm.model, "claude-sonnet-4-20250514");
        assert_eq!(profile.llm.api_key, "AGENT_SPECIFIC_KEY");
        assert_eq!(profile.llm.max_tokens, 4096);
    }

    #[test]
    fn all_fields_overridden() {
        let reg = test_registry();
        let toml = r#"
[llm.claude-sonnet]
provider = "custom"
model = "custom-model"
api_key = "CUSTOM_KEY"
max_tokens = 2048
"#;
        let profile = AgentProfile::resolve(toml, &reg).unwrap();
        assert_eq!(profile.llm.provider, "custom");
        assert_eq!(profile.llm.model, "custom-model");
        assert_eq!(profile.llm.api_key, "CUSTOM_KEY");
        assert_eq!(profile.llm.max_tokens, 2048);
    }

    #[test]
    fn max_tokens_defaults_to_8192_when_neither_has_it() {
        let reg = Registry::parse(
            r#"
[llm.bare]
provider = "anthropic"
model = "claude-sonnet-4-20250514"
api_key = "KEY"
"#,
        )
        .unwrap();
        let profile = AgentProfile::resolve("[llm.bare]\n", &reg).unwrap();
        assert_eq!(profile.llm.max_tokens, 8192);
    }

    #[test]
    fn zero_llm_entries_fails() {
        let reg = test_registry();
        let err = AgentProfile::resolve("", &reg).unwrap_err();
        assert!(err.contains("exactly one"), "{err}");
    }

    #[test]
    fn two_llm_entries_fails() {
        let mut reg = test_registry();
        reg.llm.insert(
            "gpt-4o".into(),
            RegistryLlm {
                provider: "openai".into(),
                model: "gpt-4o".into(),
                api_key: "OPENAI_KEY".into(),
                max_tokens: None,
            },
        );
        let toml = "[llm.claude-sonnet]\n[llm.gpt-4o]\n";
        let err = AgentProfile::resolve(toml, &reg).unwrap_err();
        assert!(err.contains("exactly one"), "{err}");
    }

    #[test]
    fn unknown_llm_registry_key_fails() {
        let reg = test_registry();
        let err = AgentProfile::resolve("[llm.nonexistent]\n", &reg).unwrap_err();
        assert!(err.contains("unknown llm registry key"), "{err}");
    }

    #[test]
    fn unknown_mcp_registry_key_fails() {
        let reg = test_registry();
        let toml = "[llm.claude-sonnet]\n[mcp.nonexistent]\n";
        let err = AgentProfile::resolve(toml, &reg).unwrap_err();
        assert!(err.contains("unknown mcp registry key"), "{err}");
    }

    #[test]
    fn mcp_tools_allowlist_none_means_all() {
        let reg = test_registry();
        let toml = "[llm.claude-sonnet]\n[mcp.github]\n";
        let profile = AgentProfile::resolve(toml, &reg).unwrap();
        assert_eq!(profile.mcp_servers.len(), 1);
        assert!(profile.mcp_servers[0].tool_allowlist.is_none());
    }

    #[test]
    fn mcp_tools_allowlist_empty_means_none() {
        let reg = test_registry();
        let toml = r#"
[llm.claude-sonnet]
[mcp.github]
tools = []
"#;
        let profile = AgentProfile::resolve(toml, &reg).unwrap();
        assert_eq!(
            profile.mcp_servers[0]
                .tool_allowlist
                .as_ref()
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn mcp_tools_allowlist_filters() {
        let reg = test_registry();
        let toml = r#"
[llm.claude-sonnet]
[mcp.github]
tools = ["create_pull_request", "list_issues"]
"#;
        let profile = AgentProfile::resolve(toml, &reg).unwrap();
        let tools = profile.mcp_servers[0].tool_allowlist.as_ref().unwrap();
        assert_eq!(tools, &["create_pull_request", "list_issues"]);
    }

    #[test]
    fn mcp_field_override() {
        let reg = test_registry();
        let toml = r#"
[llm.claude-sonnet]
[mcp.github]
auth_token = "AGENT_GITHUB_TOKEN"
"#;
        let profile = AgentProfile::resolve(toml, &reg).unwrap();
        assert_eq!(profile.mcp_servers[0].url, "https://mcp.github.com/sse");
        assert_eq!(profile.mcp_servers[0].auth_token, "AGENT_GITHUB_TOKEN");
    }

    #[test]
    fn multiple_mcp_servers() {
        let reg = test_registry();
        let toml = r#"
[llm.claude-sonnet]
[mcp.github]
tools = ["list_issues"]
[mcp.web-search]
"#;
        let profile = AgentProfile::resolve(toml, &reg).unwrap();
        assert_eq!(profile.mcp_servers.len(), 2);
    }

    #[test]
    fn no_mcp_servers_is_valid() {
        let reg = test_registry();
        let profile = AgentProfile::resolve("[llm.claude-sonnet]\n", &reg).unwrap();
        assert!(profile.mcp_servers.is_empty());
    }
}

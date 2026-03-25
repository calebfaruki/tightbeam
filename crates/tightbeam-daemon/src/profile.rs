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
    pub provider: tightbeam_providers::Provider,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegistryMcp {
    pub url: String,
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default)]
    pub auth_token_env: Option<String>,
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
    pub provider: tightbeam_providers::Provider,
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
    provider: Option<tightbeam_providers::Provider>,
    model: Option<String>,
    api_key: Option<String>,
    api_key_env: Option<String>,
    max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RawMcpOverride {
    url: Option<String>,
    auth_token: Option<String>,
    auth_token_env: Option<String>,
    tools: Option<Vec<String>>,
}

fn resolve_credential(
    literal: Option<&str>,
    env_name: Option<&str>,
    field_name: &str,
    env_resolver: &dyn Fn(&str) -> Option<String>,
) -> Result<String, String> {
    if let Some(val) = literal {
        return Ok(val.to_string());
    }
    if let Some(name) = env_name {
        return env_resolver(name).ok_or_else(|| {
            format!("env var '{name}' not set (required by {field_name}_env)")
        });
    }
    Err(format!(
        "must have either '{field_name}' or '{field_name}_env'"
    ))
}

impl RegistryLlm {
    pub fn resolve_api_key(
        &self,
        env_resolver: &dyn Fn(&str) -> Option<String>,
    ) -> Result<String, String> {
        resolve_credential(
            self.api_key.as_deref(),
            self.api_key_env.as_deref(),
            "api_key",
            env_resolver,
        )
    }
}

impl RegistryMcp {
    pub fn resolve_auth_token(
        &self,
        env_resolver: &dyn Fn(&str) -> Option<String>,
    ) -> Result<String, String> {
        resolve_credential(
            self.auth_token.as_deref(),
            self.auth_token_env.as_deref(),
            "auth_token",
            env_resolver,
        )
    }
}

impl AgentProfile {
    pub fn resolve(
        toml_str: &str,
        registry: &Registry,
        env_resolver: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
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

        let api_key =
            if llm_override.api_key.is_some() || llm_override.api_key_env.is_some() {
                resolve_credential(
                    llm_override.api_key.as_deref(),
                    llm_override.api_key_env.as_deref(),
                    "api_key",
                    env_resolver,
                )?
            } else {
                registry_llm.resolve_api_key(env_resolver)?
            };

        let llm = ResolvedLlm {
            provider: llm_override
                .provider
                .unwrap_or_else(|| registry_llm.provider.clone()),
            model: llm_override
                .model
                .unwrap_or_else(|| registry_llm.model.clone()),
            api_key,
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

            let auth_token = if mcp_override.auth_token.is_some()
                || mcp_override.auth_token_env.is_some()
            {
                resolve_credential(
                    mcp_override.auth_token.as_deref(),
                    mcp_override.auth_token_env.as_deref(),
                    "auth_token",
                    env_resolver,
                )?
            } else {
                registry_mcp.resolve_auth_token(env_resolver)?
            };

            mcp_servers.push(ResolvedMcp {
                name: mcp_key,
                url: mcp_override.url.unwrap_or_else(|| registry_mcp.url.clone()),
                auth_token,
                tool_allowlist: mcp_override.tools,
            });
        }

        Ok(AgentProfile { llm, mcp_servers })
    }

    pub fn load(
        path: &Path,
        registry: &Registry,
        env_resolver: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        Self::resolve(&content, registry, env_resolver)
    }
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
provider = "anthropic"
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
        assert_eq!(
            reg.llm["claude-sonnet"].provider,
            tightbeam_providers::Provider::Anthropic
        );
        assert_eq!(reg.llm["gpt-4o"].api_key.as_deref(), Some("OPENAI_API_KEY"));
        assert_eq!(reg.mcp["github"].url, "https://mcp.github.com/sse");
        assert_eq!(
            reg.mcp["web-search"].auth_token.as_deref(),
            Some("SEARCH_API_KEY")
        );
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

    fn no_env(_: &str) -> Option<String> {
        None
    }

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
        let profile = AgentProfile::resolve("[llm.claude-sonnet]\n", &reg, &no_env).unwrap();
        assert_eq!(
            profile.llm.provider,
            tightbeam_providers::Provider::Anthropic
        );
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
        let profile = AgentProfile::resolve(toml, &reg, &no_env).unwrap();
        assert_eq!(
            profile.llm.provider,
            tightbeam_providers::Provider::Anthropic
        );
        assert_eq!(profile.llm.model, "claude-sonnet-4-20250514");
        assert_eq!(profile.llm.api_key, "AGENT_SPECIFIC_KEY");
        assert_eq!(profile.llm.max_tokens, 4096);
    }

    #[test]
    fn all_fields_overridden() {
        let reg = test_registry();
        let toml = r#"
[llm.claude-sonnet]
provider = "anthropic"
model = "custom-model"
api_key = "CUSTOM_KEY"
max_tokens = 2048
"#;
        let profile = AgentProfile::resolve(toml, &reg, &no_env).unwrap();
        assert_eq!(
            profile.llm.provider,
            tightbeam_providers::Provider::Anthropic
        );
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
        let profile = AgentProfile::resolve("[llm.bare]\n", &reg, &no_env).unwrap();
        assert_eq!(profile.llm.max_tokens, 8192);
    }

    #[test]
    fn zero_llm_entries_fails() {
        let reg = test_registry();
        let err = AgentProfile::resolve("", &reg, &no_env).unwrap_err();
        assert!(err.contains("exactly one"), "{err}");
    }

    #[test]
    fn two_llm_entries_fails() {
        let mut reg = test_registry();
        reg.llm.insert(
            "gpt-4o".into(),
            RegistryLlm {
                provider: tightbeam_providers::Provider::Anthropic,
                model: "gpt-4o".into(),
                api_key: Some("OPENAI_KEY".into()),
                api_key_env: None,
                max_tokens: None,
            },
        );
        let toml = "[llm.claude-sonnet]\n[llm.gpt-4o]\n";
        let err = AgentProfile::resolve(toml, &reg, &no_env).unwrap_err();
        assert!(err.contains("exactly one"), "{err}");
    }

    #[test]
    fn unknown_llm_registry_key_fails() {
        let reg = test_registry();
        let err = AgentProfile::resolve("[llm.nonexistent]\n", &reg, &no_env).unwrap_err();
        assert!(err.contains("unknown llm registry key"), "{err}");
    }

    #[test]
    fn unknown_mcp_registry_key_fails() {
        let reg = test_registry();
        let toml = "[llm.claude-sonnet]\n[mcp.nonexistent]\n";
        let err = AgentProfile::resolve(toml, &reg, &no_env).unwrap_err();
        assert!(err.contains("unknown mcp registry key"), "{err}");
    }

    #[test]
    fn mcp_tools_allowlist_none_means_all() {
        let reg = test_registry();
        let toml = "[llm.claude-sonnet]\n[mcp.github]\n";
        let profile = AgentProfile::resolve(toml, &reg, &no_env).unwrap();
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
        let profile = AgentProfile::resolve(toml, &reg, &no_env).unwrap();
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
        let profile = AgentProfile::resolve(toml, &reg, &no_env).unwrap();
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
        let profile = AgentProfile::resolve(toml, &reg, &no_env).unwrap();
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
        let profile = AgentProfile::resolve(toml, &reg, &no_env).unwrap();
        assert_eq!(profile.mcp_servers.len(), 2);
    }

    #[test]
    fn no_mcp_servers_is_valid() {
        let reg = test_registry();
        let profile = AgentProfile::resolve("[llm.claude-sonnet]\n", &reg, &no_env).unwrap();
        assert!(profile.mcp_servers.is_empty());
    }
}

#[cfg(test)]
mod credential_resolution {
    use super::*;

    fn no_env(_name: &str) -> Option<String> {
        None
    }

    fn env_with<'a>(map: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| map.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
    }

    // --- resolve_credential ---

    #[test]
    fn literal_wins_over_env() {
        let env = env_with(&[("MY_KEY", "from-env")]);
        let result =
            resolve_credential(Some("literal"), Some("MY_KEY"), "api_key", &env).unwrap();
        assert_eq!(result, "literal");
    }

    #[test]
    fn env_resolves_when_no_literal() {
        let env = env_with(&[("MY_KEY", "from-env")]);
        let result = resolve_credential(None, Some("MY_KEY"), "api_key", &env).unwrap();
        assert_eq!(result, "from-env");
    }

    #[test]
    fn env_not_set_is_error() {
        let err = resolve_credential(None, Some("MISSING_VAR"), "api_key", &no_env).unwrap_err();
        assert!(err.contains("MISSING_VAR"), "{err}");
        assert!(err.contains("not set"), "{err}");
    }

    #[test]
    fn neither_literal_nor_env_is_error() {
        let err = resolve_credential(None, None, "api_key", &no_env).unwrap_err();
        assert!(err.contains("must have either"), "{err}");
    }

    // --- RegistryLlm.resolve_api_key ---

    #[test]
    fn registry_api_key_literal() {
        let llm = RegistryLlm {
            provider: tightbeam_providers::Provider::Anthropic,
            model: "m".into(),
            api_key: Some("literal-key".into()),
            api_key_env: Some("SHOULD_NOT_READ".into()),
            max_tokens: None,
        };
        assert_eq!(llm.resolve_api_key(&no_env).unwrap(), "literal-key");
    }

    #[test]
    fn registry_api_key_from_env() {
        let env = env_with(&[("MY_API_KEY", "env-key")]);
        let llm = RegistryLlm {
            provider: tightbeam_providers::Provider::Anthropic,
            model: "m".into(),
            api_key: None,
            api_key_env: Some("MY_API_KEY".into()),
            max_tokens: None,
        };
        assert_eq!(llm.resolve_api_key(&env).unwrap(), "env-key");
    }

    #[test]
    fn registry_api_key_neither_is_error() {
        let llm = RegistryLlm {
            provider: tightbeam_providers::Provider::Anthropic,
            model: "m".into(),
            api_key: None,
            api_key_env: None,
            max_tokens: None,
        };
        let err = llm.resolve_api_key(&no_env).unwrap_err();
        assert!(err.contains("must have either"), "{err}");
    }

    // --- RegistryMcp.resolve_auth_token ---

    #[test]
    fn registry_auth_token_literal() {
        let mcp = RegistryMcp {
            url: "http://x".into(),
            auth_token: Some("literal-token".into()),
            auth_token_env: None,
        };
        assert_eq!(mcp.resolve_auth_token(&no_env).unwrap(), "literal-token");
    }

    #[test]
    fn registry_auth_token_from_env() {
        let env = env_with(&[("MY_TOKEN", "env-token")]);
        let mcp = RegistryMcp {
            url: "http://x".into(),
            auth_token: None,
            auth_token_env: Some("MY_TOKEN".into()),
        };
        assert_eq!(mcp.resolve_auth_token(&env).unwrap(), "env-token");
    }

    #[test]
    fn registry_auth_token_neither_is_error() {
        let mcp = RegistryMcp {
            url: "http://x".into(),
            auth_token: None,
            auth_token_env: None,
        };
        let err = mcp.resolve_auth_token(&no_env).unwrap_err();
        assert!(err.contains("must have either"), "{err}");
    }

    // --- Agent-level credential resolution ---

    fn env_registry() -> Registry {
        Registry {
            llm: [("claude".into(), RegistryLlm {
                provider: tightbeam_providers::Provider::Anthropic,
                model: "claude-sonnet".into(),
                api_key: Some("registry-key".into()),
                api_key_env: None,
                max_tokens: None,
            })].into(),
            mcp: [("gh".into(), RegistryMcp {
                url: "http://gh".into(),
                auth_token: Some("registry-token".into()),
                auth_token_env: None,
            })].into(),
        }
    }

    #[test]
    fn agent_override_api_key_env_wins_over_registry() {
        let reg = env_registry();
        let env = env_with(&[("AGENT_KEY", "agent-env-key")]);
        let toml = r#"
[llm.claude]
api_key_env = "AGENT_KEY"
"#;
        let profile = AgentProfile::resolve(toml, &reg, &env).unwrap();
        assert_eq!(profile.llm.api_key, "agent-env-key");
    }

    #[test]
    fn agent_override_api_key_literal_wins_over_registry() {
        let reg = env_registry();
        let toml = r#"
[llm.claude]
api_key = "agent-literal-key"
"#;
        let profile = AgentProfile::resolve(toml, &reg, &no_env).unwrap();
        assert_eq!(profile.llm.api_key, "agent-literal-key");
    }

    #[test]
    fn agent_defers_credential_to_registry() {
        let reg = env_registry();
        let toml = "[llm.claude]\n";
        let profile = AgentProfile::resolve(toml, &reg, &no_env).unwrap();
        assert_eq!(profile.llm.api_key, "registry-key");
    }

    #[test]
    fn agent_override_auth_token_env_wins_over_registry() {
        let reg = env_registry();
        let env = env_with(&[("AGENT_TOKEN", "agent-env-token")]);
        let toml = r#"
[llm.claude]
[mcp.gh]
auth_token_env = "AGENT_TOKEN"
"#;
        let profile = AgentProfile::resolve(toml, &reg, &env).unwrap();
        assert_eq!(profile.mcp_servers[0].auth_token, "agent-env-token");
    }

    #[test]
    fn agent_defers_mcp_credential_to_registry() {
        let reg = env_registry();
        let toml = "[llm.claude]\n[mcp.gh]\n";
        let profile = AgentProfile::resolve(toml, &reg, &no_env).unwrap();
        assert_eq!(profile.mcp_servers[0].auth_token, "registry-token");
    }

    #[test]
    fn full_profile_resolution_with_env_vars() {
        let reg = Registry {
            llm: [("claude".into(), RegistryLlm {
                provider: tightbeam_providers::Provider::Anthropic,
                model: "claude-sonnet".into(),
                api_key: None,
                api_key_env: Some("REG_API_KEY".into()),
                max_tokens: Some(4096),
            })].into(),
            mcp: [("gh".into(), RegistryMcp {
                url: "http://gh".into(),
                auth_token: None,
                auth_token_env: Some("REG_GH_TOKEN".into()),
            })].into(),
        };
        let env = env_with(&[("REG_API_KEY", "resolved-api"), ("REG_GH_TOKEN", "resolved-gh")]);
        let toml = "[llm.claude]\n[mcp.gh]\n";
        let profile = AgentProfile::resolve(toml, &reg, &env).unwrap();
        assert_eq!(profile.llm.api_key, "resolved-api");
        assert_eq!(profile.mcp_servers[0].auth_token, "resolved-gh");
    }
}

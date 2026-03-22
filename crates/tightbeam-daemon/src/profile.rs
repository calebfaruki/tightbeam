use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct AgentProfile {
    pub provider: ProviderConfig,
    pub socket: SocketConfig,
    pub log: LogConfig,
}

#[derive(Debug, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub model: String,
    pub api_key_env: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

fn default_max_tokens() -> u32 {
    8192
}

#[derive(Debug, Deserialize)]
pub struct SocketConfig {
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct LogConfig {
    pub path: String,
}

impl AgentProfile {
    pub fn parse(toml_str: &str) -> Result<Self, String> {
        toml::from_str(toml_str).map_err(|e| format!("invalid profile: {e}"))
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        Self::parse(&content)
    }
}

#[cfg(test)]
mod profile_loading {
    use super::*;

    #[test]
    fn valid_profile_parses() {
        let toml = r#"
[provider]
name = "claude"
model = "claude-sonnet-4-20250514"
api_key_env = "ANTHROPIC_API_KEY"
max_tokens = 8192

[socket]
path = "repairapp-dev.sock"

[log]
path = "repairapp-dev/"
"#;
        let profile = AgentProfile::parse(toml).unwrap();
        assert_eq!(profile.provider.name, "claude");
        assert_eq!(profile.provider.model, "claude-sonnet-4-20250514");
        assert_eq!(profile.provider.api_key_env, "ANTHROPIC_API_KEY");
        assert_eq!(profile.provider.max_tokens, 8192);
        assert_eq!(profile.socket.path, "repairapp-dev.sock");
        assert_eq!(profile.log.path, "repairapp-dev/");
    }

    #[test]
    fn max_tokens_defaults_to_8192() {
        let toml = r#"
[provider]
name = "claude"
model = "claude-sonnet-4-20250514"
api_key_env = "ANTHROPIC_API_KEY"

[socket]
path = "test.sock"

[log]
path = "test/"
"#;
        let profile = AgentProfile::parse(toml).unwrap();
        assert_eq!(profile.provider.max_tokens, 8192);
    }

    #[test]
    fn missing_provider_fails() {
        let toml = r#"
[socket]
path = "test.sock"

[log]
path = "test/"
"#;
        assert!(AgentProfile::parse(toml).is_err());
    }

    #[test]
    fn missing_socket_fails() {
        let toml = r#"
[provider]
name = "claude"
model = "claude-sonnet-4-20250514"
api_key_env = "ANTHROPIC_API_KEY"

[log]
path = "test/"
"#;
        assert!(AgentProfile::parse(toml).is_err());
    }

    #[test]
    fn missing_log_fails() {
        let toml = r#"
[provider]
name = "claude"
model = "claude-sonnet-4-20250514"
api_key_env = "ANTHROPIC_API_KEY"

[socket]
path = "test.sock"
"#;
        assert!(AgentProfile::parse(toml).is_err());
    }

    #[test]
    fn missing_required_provider_field_fails() {
        let toml = r#"
[provider]
name = "claude"
api_key_env = "ANTHROPIC_API_KEY"

[socket]
path = "test.sock"

[log]
path = "test/"
"#;
        let err = AgentProfile::parse(toml).unwrap_err();
        assert!(err.contains("model"));
    }
}

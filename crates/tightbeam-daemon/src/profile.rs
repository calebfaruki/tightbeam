use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

// --- TOML config structs (deserialization) ---

#[derive(Debug, Deserialize)]
struct AgentConfigToml {
    llm: LlmConfig,
    #[serde(default)]
    mcp: HashMap<String, McpConfig>,
}

#[derive(Debug, Deserialize)]
struct LlmConfig {
    provider: tightbeam_providers::Provider,
    model: String,
    api_key_file: String,
    max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct McpConfig {
    url: String,
    auth_token_file: String,
    tools: Option<Vec<String>>,
}

// --- Resolved runtime structs ---

#[derive(Debug)]
pub struct AgentConfig {
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

fn read_secret_file(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("failed to read secret {path}: {e}"))
}

impl AgentConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        Self::resolve(&content)
    }

    pub fn resolve(toml_str: &str) -> Result<Self, String> {
        let config: AgentConfigToml =
            toml::from_str(toml_str).map_err(|e| format!("invalid agent config: {e}"))?;

        let api_key = read_secret_file(&config.llm.api_key_file)?;
        let llm = ResolvedLlm {
            provider: config.llm.provider,
            model: config.llm.model,
            api_key,
            max_tokens: config.llm.max_tokens.unwrap_or(8192),
        };

        let mut mcp_servers = Vec::new();
        for (name, mcp) in config.mcp {
            let auth_token = read_secret_file(&mcp.auth_token_file)?;
            mcp_servers.push(ResolvedMcp {
                name,
                url: mcp.url,
                auth_token,
                tool_allowlist: mcp.tools,
            });
        }

        Ok(AgentConfig { llm, mcp_servers })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_secret(dir: &std::path::Path, name: &str, value: &str) -> String {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(value.as_bytes()).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn resolves_llm_config_with_secret_file() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = write_secret(dir.path(), "api_key", "sk-ant-test-123");
        let toml = format!(
            r#"
[llm]
provider = "anthropic"
model = "claude-sonnet-4-20250514"
api_key_file = "{key_path}"
"#
        );
        let config = AgentConfig::resolve(&toml).unwrap();
        assert_eq!(
            config.llm.provider,
            tightbeam_providers::Provider::Anthropic
        );
        assert_eq!(config.llm.model, "claude-sonnet-4-20250514");
        assert_eq!(config.llm.api_key, "sk-ant-test-123");
        assert_eq!(config.llm.max_tokens, 8192);
    }

    #[test]
    fn max_tokens_override() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = write_secret(dir.path(), "api_key", "key");
        let toml = format!(
            r#"
[llm]
provider = "anthropic"
model = "m"
api_key_file = "{key_path}"
max_tokens = 4096
"#
        );
        let config = AgentConfig::resolve(&toml).unwrap();
        assert_eq!(config.llm.max_tokens, 4096);
    }

    #[test]
    fn resolves_mcp_with_secret_file() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = write_secret(dir.path(), "api_key", "key");
        let token_path = write_secret(dir.path(), "gh_token", "ghp-test-456");
        let toml = format!(
            r#"
[llm]
provider = "anthropic"
model = "m"
api_key_file = "{key_path}"

[mcp.github]
url = "https://mcp.github.com/sse"
auth_token_file = "{token_path}"
tools = ["create_pull_request", "list_issues"]
"#
        );
        let config = AgentConfig::resolve(&toml).unwrap();
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(config.mcp_servers[0].name, "github");
        assert_eq!(config.mcp_servers[0].auth_token, "ghp-test-456");
        assert_eq!(
            config.mcp_servers[0].tool_allowlist.as_ref().unwrap(),
            &["create_pull_request", "list_issues"]
        );
    }

    #[test]
    fn missing_secret_file_is_error() {
        let toml = r#"
[llm]
provider = "anthropic"
model = "m"
api_key_file = "/nonexistent/secret/path"
"#;
        let err = AgentConfig::resolve(toml).unwrap_err();
        assert!(err.contains("failed to read secret"), "{err}");
        assert!(err.contains("/nonexistent/secret/path"), "{err}");
    }

    #[test]
    fn missing_llm_section_is_error() {
        let err = AgentConfig::resolve("").unwrap_err();
        assert!(err.contains("llm"), "{err}");
    }

    #[test]
    fn no_mcp_servers_is_valid() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = write_secret(dir.path(), "api_key", "key");
        let toml = format!(
            r#"
[llm]
provider = "anthropic"
model = "m"
api_key_file = "{key_path}"
"#
        );
        let config = AgentConfig::resolve(&toml).unwrap();
        assert!(config.mcp_servers.is_empty());
    }

    #[test]
    fn multiple_mcp_servers() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = write_secret(dir.path(), "api_key", "key");
        let gh_path = write_secret(dir.path(), "gh_token", "gh");
        let search_path = write_secret(dir.path(), "search_token", "search");
        let toml = format!(
            r#"
[llm]
provider = "anthropic"
model = "m"
api_key_file = "{key_path}"

[mcp.github]
url = "https://gh"
auth_token_file = "{gh_path}"

[mcp.search]
url = "https://search"
auth_token_file = "{search_path}"
"#
        );
        let config = AgentConfig::resolve(&toml).unwrap();
        assert_eq!(config.mcp_servers.len(), 2);
    }

    #[test]
    fn mcp_tools_omitted_means_all() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = write_secret(dir.path(), "api_key", "key");
        let token_path = write_secret(dir.path(), "token", "tok");
        let toml = format!(
            r#"
[llm]
provider = "anthropic"
model = "m"
api_key_file = "{key_path}"

[mcp.github]
url = "https://gh"
auth_token_file = "{token_path}"
"#
        );
        let config = AgentConfig::resolve(&toml).unwrap();
        assert!(config.mcp_servers[0].tool_allowlist.is_none());
    }

    #[test]
    fn secret_file_whitespace_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = write_secret(dir.path(), "api_key", "  sk-ant-key  \n");
        let toml = format!(
            r#"
[llm]
provider = "anthropic"
model = "m"
api_key_file = "{key_path}"
"#
        );
        let config = AgentConfig::resolve(&toml).unwrap();
        assert_eq!(config.llm.api_key, "sk-ant-key");
    }
}

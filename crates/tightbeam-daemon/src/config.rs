use std::path::Path;

pub(crate) const LLM_SECRETS_DIR: &str = "/run/secrets/llm";
pub(crate) const MCP_CONFIG_DIR: &str = "/run/secrets/mcp";

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

fn read_secret_file(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("failed to read {}: {e}", path.display()))
}

fn load_llm_from_mount(dir: &Path) -> Result<ResolvedLlm, String> {
    let provider_str = read_secret_file(&dir.join("provider"))?;
    let provider: tightbeam_providers::Provider =
        serde_json::from_str(&format!("\"{provider_str}\""))
            .map_err(|e| format!("invalid provider \"{provider_str}\": {e}"))?;

    let model = read_secret_file(&dir.join("model"))?;
    let api_key = read_secret_file(&dir.join("api-key"))?;

    let max_tokens = match read_secret_file(&dir.join("max-tokens")) {
        Ok(val) => val.parse().unwrap_or(8192),
        Err(_) => 8192,
    };

    Ok(ResolvedLlm {
        provider,
        model,
        api_key,
        max_tokens,
    })
}

fn load_mcp_from_dir(dir: &Path) -> Vec<ResolvedMcp> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut servers = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let subdir = entry.path();

        let url = match read_secret_file(&subdir.join("url")) {
            Ok(u) if !u.is_empty() => u,
            _ => {
                eprintln!("tightbeam: mcp/{name}: missing or empty url, skipping");
                continue;
            }
        };

        let auth_token = read_secret_file(&subdir.join("auth_token")).unwrap_or_default();

        let tool_allowlist = match std::fs::read_to_string(subdir.join("tools")) {
            Ok(content) => {
                let tools: Vec<String> = content
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect();
                Some(tools)
            }
            Err(_) => None,
        };

        servers.push(ResolvedMcp {
            name,
            url,
            auth_token,
            tool_allowlist,
        });
    }
    servers
}

impl AgentConfig {
    pub fn load() -> Result<Self, String> {
        let llm = load_llm_from_mount(Path::new(LLM_SECRETS_DIR))?;
        let mcp_servers = load_mcp_from_dir(Path::new(MCP_CONFIG_DIR));
        Ok(AgentConfig { llm, mcp_servers })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(dir: &std::path::Path, name: &str, value: &str) {
        std::fs::write(dir.join(name), value).unwrap();
    }

    fn write_llm_mount(dir: &std::path::Path, provider: &str, model: &str, api_key: &str) {
        write_file(dir, "provider", provider);
        write_file(dir, "model", model);
        write_file(dir, "api-key", api_key);
    }

    fn make_mcp_server(dir: &std::path::Path, name: &str, url: &str) -> std::path::PathBuf {
        let subdir = dir.join(name);
        std::fs::create_dir_all(&subdir).unwrap();
        write_file(&subdir, "url", url);
        subdir
    }

    // --- LLM mount tests ---

    #[test]
    fn loads_llm_from_mount() {
        let dir = tempfile::tempdir().unwrap();
        write_llm_mount(
            dir.path(),
            "anthropic",
            "claude-sonnet-4-20250514",
            "sk-ant-test-123",
        );

        let llm = load_llm_from_mount(dir.path()).unwrap();
        assert_eq!(llm.provider, tightbeam_providers::Provider::Anthropic);
        assert_eq!(llm.model, "claude-sonnet-4-20250514");
        assert_eq!(llm.api_key, "sk-ant-test-123");
        assert_eq!(llm.max_tokens, 8192);
    }

    #[test]
    fn max_tokens_from_mount() {
        let dir = tempfile::tempdir().unwrap();
        write_llm_mount(dir.path(), "anthropic", "m", "key");
        write_file(dir.path(), "max-tokens", "4096");

        let llm = load_llm_from_mount(dir.path()).unwrap();
        assert_eq!(llm.max_tokens, 4096);
    }

    #[test]
    fn max_tokens_defaults_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        write_llm_mount(dir.path(), "anthropic", "m", "key");

        let llm = load_llm_from_mount(dir.path()).unwrap();
        assert_eq!(llm.max_tokens, 8192);
    }

    #[test]
    fn max_tokens_defaults_when_unparseable() {
        let dir = tempfile::tempdir().unwrap();
        write_llm_mount(dir.path(), "anthropic", "m", "key");
        write_file(dir.path(), "max-tokens", "not-a-number");

        let llm = load_llm_from_mount(dir.path()).unwrap();
        assert_eq!(llm.max_tokens, 8192);
    }

    #[test]
    fn missing_provider_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "model", "m");
        write_file(dir.path(), "api-key", "key");

        let err = load_llm_from_mount(dir.path()).unwrap_err();
        assert!(err.contains("provider"), "{err}");
    }

    #[test]
    fn missing_model_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "provider", "anthropic");
        write_file(dir.path(), "api-key", "key");

        let err = load_llm_from_mount(dir.path()).unwrap_err();
        assert!(err.contains("model"), "{err}");
    }

    #[test]
    fn missing_api_key_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "provider", "anthropic");
        write_file(dir.path(), "model", "m");

        let err = load_llm_from_mount(dir.path()).unwrap_err();
        assert!(err.contains("api-key"), "{err}");
    }

    #[test]
    fn invalid_provider_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_llm_mount(dir.path(), "not-a-provider", "m", "key");

        let err = load_llm_from_mount(dir.path()).unwrap_err();
        assert!(err.contains("invalid provider"), "{err}");
    }

    #[test]
    fn mount_files_whitespace_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        write_llm_mount(
            dir.path(),
            "  anthropic\n",
            "  my-model  \n",
            "  sk-key  \n",
        );

        let llm = load_llm_from_mount(dir.path()).unwrap();
        assert_eq!(llm.provider, tightbeam_providers::Provider::Anthropic);
        assert_eq!(llm.model, "my-model");
        assert_eq!(llm.api_key, "sk-key");
    }

    // --- MCP directory tests ---

    #[test]
    fn loads_mcp_from_dir() {
        let dir = tempfile::tempdir().unwrap();
        let subdir = make_mcp_server(dir.path(), "github", "https://mcp.github.com/sse");
        write_file(&subdir, "auth_token", "ghp-test-456");
        write_file(&subdir, "tools", "create_pull_request\nlist_issues\n");

        let servers = load_mcp_from_dir(dir.path());
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "github");
        assert_eq!(servers[0].url, "https://mcp.github.com/sse");
        assert_eq!(servers[0].auth_token, "ghp-test-456");
        assert_eq!(
            servers[0].tool_allowlist.as_ref().unwrap(),
            &["create_pull_request", "list_issues"]
        );
    }

    #[test]
    fn mcp_url_only() {
        let dir = tempfile::tempdir().unwrap();
        make_mcp_server(dir.path(), "filesystem", "http://localhost:9100/sse");

        let servers = load_mcp_from_dir(dir.path());
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].auth_token, "");
        assert!(servers[0].tool_allowlist.is_none());
    }

    #[test]
    fn mcp_missing_url_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let subdir = dir.path().join("broken");
        std::fs::create_dir_all(&subdir).unwrap();

        let servers = load_mcp_from_dir(dir.path());
        assert!(servers.is_empty());
    }

    #[test]
    fn mcp_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let servers = load_mcp_from_dir(dir.path());
        assert!(servers.is_empty());
    }

    #[test]
    fn mcp_nonexistent_dir() {
        let servers = load_mcp_from_dir(Path::new("/nonexistent/mcp/dir"));
        assert!(servers.is_empty());
    }

    #[test]
    fn mcp_multiple_servers() {
        let dir = tempfile::tempdir().unwrap();
        make_mcp_server(dir.path(), "github", "https://gh");
        make_mcp_server(dir.path(), "search", "https://search");

        let servers = load_mcp_from_dir(dir.path());
        assert_eq!(servers.len(), 2);
    }

    #[test]
    fn mcp_tools_file_whitespace_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let subdir = make_mcp_server(dir.path(), "gh", "https://gh");
        write_file(&subdir, "tools", "  tool_a  \n\n  tool_b  \n");

        let servers = load_mcp_from_dir(dir.path());
        assert_eq!(
            servers[0].tool_allowlist.as_ref().unwrap(),
            &["tool_a", "tool_b"]
        );
    }

    #[test]
    fn mcp_auth_token_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let subdir = make_mcp_server(dir.path(), "gh", "https://gh");
        write_file(&subdir, "auth_token", "  token-value  \n");

        let servers = load_mcp_from_dir(dir.path());
        assert_eq!(servers[0].auth_token, "token-value");
    }

    #[test]
    fn mcp_empty_url_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let subdir = dir.path().join("broken");
        std::fs::create_dir_all(&subdir).unwrap();
        write_file(&subdir, "url", "");

        let servers = load_mcp_from_dir(dir.path());
        assert!(servers.is_empty());
    }

    #[test]
    fn mcp_regular_files_in_dir_ignored() {
        let dir = tempfile::tempdir().unwrap();
        make_mcp_server(dir.path(), "github", "https://gh");
        write_file(dir.path(), "some-file.txt", "not a server");

        let servers = load_mcp_from_dir(dir.path());
        assert_eq!(servers.len(), 1);
    }
}

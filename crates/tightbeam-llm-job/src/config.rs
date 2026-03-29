use std::path::Path;
use tightbeam_providers::ProviderConfig;

pub(crate) const DEFAULT_SECRETS_DIR: &str = "/run/secrets/llm";

pub(crate) fn read_secret_file(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("failed to read {}: {e}", path.display()))
}

pub(crate) fn load_config(
    secrets_dir: &Path,
) -> Result<(tightbeam_providers::Provider, ProviderConfig), String> {
    let provider_str = read_secret_file(&secrets_dir.join("provider"))?;
    let provider: tightbeam_providers::Provider =
        serde_json::from_str(&format!("\"{provider_str}\""))
            .map_err(|e| format!("invalid provider \"{provider_str}\": {e}"))?;

    let model = read_secret_file(&secrets_dir.join("model"))?;
    let api_key = read_secret_file(&secrets_dir.join("api-key"))?;

    let max_tokens = match read_secret_file(&secrets_dir.join("max-tokens")) {
        Ok(val) => val.parse().unwrap_or(8192),
        Err(_) => 8192,
    };

    let config = ProviderConfig {
        model,
        api_key,
        max_tokens,
    };

    Ok((provider, config))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(dir: &Path, name: &str, value: &str) {
        std::fs::write(dir.join(name), value).unwrap();
    }

    fn write_valid_secrets(dir: &Path) {
        write_file(dir, "provider", "anthropic");
        write_file(dir, "model", "claude-sonnet-4-20250514");
        write_file(dir, "api-key", "sk-ant-test-123");
    }

    #[test]
    fn read_secret_file_trims_whitespace() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "test", "  hello world  \n");
        let result = read_secret_file(&tmp.path().join("test")).unwrap();
        assert_eq!(result, "hello world");
    }

    #[test]
    fn read_secret_file_missing_file_errors() {
        let result = read_secret_file(Path::new("/nonexistent/file"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("failed to read"));
    }

    #[test]
    fn load_config_valid_secrets() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_valid_secrets(tmp.path());

        let (provider, config) = load_config(tmp.path()).unwrap();
        assert_eq!(provider, tightbeam_providers::Provider::Anthropic);
        assert_eq!(config.model, "claude-sonnet-4-20250514");
        assert_eq!(config.api_key, "sk-ant-test-123");
        assert_eq!(config.max_tokens, 8192);
    }

    #[test]
    fn load_config_missing_api_key_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "provider", "anthropic");
        write_file(tmp.path(), "model", "m");

        match load_config(tmp.path()) {
            Err(e) => assert!(e.contains("api-key"), "{e}"),
            Ok(_) => panic!("expected error for missing api-key"),
        }
    }

    #[test]
    fn load_config_invalid_provider_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "provider", "banana");
        write_file(tmp.path(), "model", "m");
        write_file(tmp.path(), "api-key", "key");

        match load_config(tmp.path()) {
            Err(e) => assert!(e.contains("invalid provider"), "{e}"),
            Ok(_) => panic!("expected error for invalid provider"),
        }
    }

    #[test]
    fn load_config_max_tokens_defaults_when_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_valid_secrets(tmp.path());

        let (_, config) = load_config(tmp.path()).unwrap();
        assert_eq!(config.max_tokens, 8192);
    }

    #[test]
    fn load_config_max_tokens_custom() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_valid_secrets(tmp.path());
        write_file(tmp.path(), "max-tokens", "4096");

        let (_, config) = load_config(tmp.path()).unwrap();
        assert_eq!(config.max_tokens, 4096);
    }

    #[test]
    fn load_config_max_tokens_invalid_defaults() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_valid_secrets(tmp.path());
        write_file(tmp.path(), "max-tokens", "not-a-number");

        let (_, config) = load_config(tmp.path()).unwrap();
        assert_eq!(config.max_tokens, 8192);
    }
}

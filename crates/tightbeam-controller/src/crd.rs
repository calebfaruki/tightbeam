use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "tightbeam.dev",
    version = "v1",
    kind = "TightbeamModel",
    namespaced
)]
pub struct TightbeamModelSpec {
    pub provider: String,
    pub model: String,
    #[serde(rename = "secretName")]
    pub secret_name: String,
    #[serde(rename = "maxTokens", default = "default_max_tokens")]
    pub max_tokens: u32,
    pub image: String,
    #[serde(rename = "idleTimeout", default = "default_idle_timeout")]
    pub idle_timeout: u32,
    #[serde(default)]
    pub description: String,
}

fn default_max_tokens() -> u32 {
    8192
}

fn default_idle_timeout() -> u32 {
    300
}

#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "tightbeam.dev",
    version = "v1",
    kind = "TightbeamChannel",
    namespaced
)]
pub struct TightbeamChannelSpec {
    #[serde(rename = "type")]
    pub channel_type: String,
    #[serde(rename = "secretName")]
    pub secret_name: String,
    pub image: String,
    #[serde(rename = "targetModel", default)]
    pub target_model: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::Resource;

    #[test]
    fn model_spec_serializes() {
        let spec = TightbeamModelSpec {
            provider: "anthropic".into(),
            model: "claude-sonnet-4-20250514".into(),
            secret_name: "llm-anthropic-key".into(),
            max_tokens: 8192,
            image: "ghcr.io/calebfaruki/tightbeam-llm-job:latest".into(),
            idle_timeout: 300,
            description: "Fast model".into(),
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains("\"secretName\":\"llm-anthropic-key\""));
        assert!(json.contains("\"maxTokens\":8192"));
    }

    #[test]
    fn model_spec_deserializes_with_defaults() {
        let json = r#"{
            "provider": "anthropic",
            "model": "claude-sonnet-4-20250514",
            "secretName": "llm-key",
            "image": "ghcr.io/test:latest"
        }"#;
        let spec: TightbeamModelSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.max_tokens, 8192);
        assert_eq!(spec.idle_timeout, 300);
        assert!(spec.description.is_empty());
    }

    #[test]
    fn channel_spec_serializes() {
        let spec = TightbeamChannelSpec {
            channel_type: "discord".into(),
            secret_name: "discord-bot-token".into(),
            image: "ghcr.io/calebfaruki/tightbeam-channel-discord:latest".into(),
            target_model: "claude-sonnet".into(),
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains("\"type\":\"discord\""));
        assert!(json.contains("\"secretName\":\"discord-bot-token\""));
        assert!(json.contains("\"targetModel\":\"claude-sonnet\""));
    }

    #[test]
    fn channel_spec_deserializes_with_defaults() {
        let json = r#"{
            "type": "discord",
            "secretName": "token",
            "image": "ghcr.io/test:latest"
        }"#;
        let spec: TightbeamChannelSpec = serde_json::from_str(json).unwrap();
        assert!(spec.target_model.is_empty());
    }

    #[test]
    fn model_crd_generates_correct_kind() {
        assert_eq!(TightbeamModel::kind(&()), "TightbeamModel");
        assert_eq!(TightbeamModel::group(&()), "tightbeam.dev");
        assert_eq!(TightbeamModel::version(&()), "v1");
    }

    #[test]
    fn channel_crd_generates_correct_kind() {
        assert_eq!(TightbeamChannel::kind(&()), "TightbeamChannel");
        assert_eq!(TightbeamChannel::group(&()), "tightbeam.dev");
        assert_eq!(TightbeamChannel::version(&()), "v1");
    }
}

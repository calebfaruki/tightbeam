use crate::crd::{TightbeamChannelSpec, TightbeamModelSpec};
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, EnvVar, PodSpec, PodTemplateSpec, SecretVolumeSource, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::collections::BTreeMap;

fn job_labels(type_label: &str, name_key: &str, name_value: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert("app.kubernetes.io/part-of".into(), "sycophant".into());
    labels.insert("tightbeam.dev/type".into(), type_label.into());
    labels.insert(format!("tightbeam.dev/{name_key}"), name_value.into());
    labels
}

fn secret_volume(volume_name: &str, mount_path: &str, secret_name: &str) -> (Volume, VolumeMount) {
    let volume = Volume {
        name: volume_name.into(),
        secret: Some(SecretVolumeSource {
            secret_name: Some(secret_name.into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mount = VolumeMount {
        name: volume_name.into(),
        mount_path: mount_path.into(),
        read_only: Some(true),
        ..Default::default()
    };
    (volume, mount)
}

pub fn build_llm_job(
    model_name: &str,
    spec: &TightbeamModelSpec,
    controller_addr: &str,
    namespace: &str,
    session_id: &str,
) -> Job {
    let job_name = format!("tightbeam-llm-{model_name}-{session_id}");
    let labels = job_labels("llm", "model", model_name);
    let (volume, mount) = secret_volume("llm-secrets", "/run/secrets/llm", &spec.secret_name);

    Job {
        metadata: ObjectMeta {
            name: Some(job_name),
            namespace: Some(namespace.into()),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(JobSpec {
            ttl_seconds_after_finished: Some(30),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    restart_policy: Some("Never".into()),
                    containers: vec![Container {
                        name: "llm".into(),
                        image: Some(spec.image.clone()),
                        args: Some(vec!["--idle-timeout".into(), spec.idle_timeout.to_string()]),
                        env: Some(vec![
                            EnvVar {
                                name: "TIGHTBEAM_CONTROLLER_ADDR".into(),
                                value: Some(controller_addr.into()),
                                ..Default::default()
                            },
                            EnvVar {
                                name: "TIGHTBEAM_MODEL_NAME".into(),
                                value: Some(model_name.into()),
                                ..Default::default()
                            },
                            EnvVar {
                                name: "TIGHTBEAM_JOB_ID".into(),
                                value: Some(format!("llm-{model_name}-{session_id}")),
                                ..Default::default()
                            },
                        ]),
                        volume_mounts: Some(vec![mount]),
                        ..Default::default()
                    }],
                    volumes: Some(vec![volume]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub async fn create_llm_job(
    client: &kube::Client,
    model_name: &str,
    spec: &TightbeamModelSpec,
    controller_addr: &str,
    namespace: &str,
) -> Result<String, kube::Error> {
    let session_id = format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    let job = build_llm_job(model_name, spec, controller_addr, namespace, &session_id);
    let job_name = job.metadata.name.clone().unwrap_or_default();

    let api: kube::Api<Job> = kube::Api::namespaced(client.clone(), namespace);
    api.create(&kube::api::PostParams::default(), &job).await?;

    tracing::info!("created LLM Job {job_name} in namespace {namespace}");
    Ok(job_name)
}

pub fn build_channel_job(
    channel_name: &str,
    spec: &TightbeamChannelSpec,
    controller_addr: &str,
    namespace: &str,
    session_id: &str,
) -> Job {
    let job_name = format!("tightbeam-channel-{channel_name}-{session_id}");
    let labels = job_labels("channel", "channel", channel_name);
    let (volume, mount) =
        secret_volume("channel-secrets", "/run/secrets/channel", &spec.secret_name);

    Job {
        metadata: ObjectMeta {
            name: Some(job_name),
            namespace: Some(namespace.into()),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(JobSpec {
            ttl_seconds_after_finished: Some(30),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    restart_policy: Some("OnFailure".into()),
                    containers: vec![Container {
                        name: "channel".into(),
                        image: Some(spec.image.clone()),
                        env: Some(vec![
                            EnvVar {
                                name: "TIGHTBEAM_CONTROLLER_ADDR".into(),
                                value: Some(controller_addr.into()),
                                ..Default::default()
                            },
                            EnvVar {
                                name: "TIGHTBEAM_CHANNEL_NAME".into(),
                                value: Some(channel_name.into()),
                                ..Default::default()
                            },
                        ]),
                        volume_mounts: Some(vec![mount]),
                        ..Default::default()
                    }],
                    volumes: Some(vec![volume]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_model_spec() -> TightbeamModelSpec {
        TightbeamModelSpec {
            provider: "anthropic".into(),
            model: "claude-sonnet-4-20250514".into(),
            secret_name: "llm-anthropic-key".into(),
            max_tokens: 8192,
            image: "ghcr.io/calebfaruki/tightbeam-llm-job:latest".into(),
            idle_timeout: 300,
            description: "Fast model".into(),
        }
    }

    fn sample_channel_spec() -> TightbeamChannelSpec {
        TightbeamChannelSpec {
            channel_type: "discord".into(),
            secret_name: "discord-bot-token".into(),
            image: "ghcr.io/calebfaruki/tightbeam-channel-discord:latest".into(),
            target_model: "claude-sonnet".into(),
        }
    }

    #[test]
    fn llm_job_has_correct_name() {
        let job = build_llm_job(
            "claude-sonnet",
            &sample_model_spec(),
            "http://controller:9090",
            "workspace-test",
            "abc123",
        );
        assert_eq!(
            job.metadata.name.unwrap(),
            "tightbeam-llm-claude-sonnet-abc123"
        );
        assert_eq!(job.metadata.namespace.unwrap(), "workspace-test");
    }

    #[test]
    fn llm_job_has_correct_labels() {
        let job = build_llm_job(
            "claude-sonnet",
            &sample_model_spec(),
            "http://controller:9090",
            "ws",
            "s1",
        );
        let labels = job.metadata.labels.unwrap();
        assert_eq!(labels["app.kubernetes.io/part-of"], "sycophant");
        assert_eq!(labels["tightbeam.dev/type"], "llm");
        assert_eq!(labels["tightbeam.dev/model"], "claude-sonnet");
    }

    #[test]
    fn llm_job_mounts_secret_by_name() {
        let job = build_llm_job("m", &sample_model_spec(), "http://c:9090", "ns", "s1");
        let pod_spec = job.spec.unwrap().template.spec.unwrap();
        let volume = &pod_spec.volumes.unwrap()[0];
        assert_eq!(volume.name, "llm-secrets");
        assert_eq!(
            volume.secret.as_ref().unwrap().secret_name.as_deref(),
            Some("llm-anthropic-key")
        );
        let mount = &pod_spec.containers[0].volume_mounts.as_ref().unwrap()[0];
        assert_eq!(mount.name, "llm-secrets");
        assert_eq!(mount.mount_path, "/run/secrets/llm");
        assert_eq!(mount.read_only, Some(true));
    }

    #[test]
    fn llm_job_pod_template_has_labels() {
        let job = build_llm_job(
            "claude-sonnet",
            &sample_model_spec(),
            "http://c:9090",
            "ns",
            "s1",
        );
        let template_labels = job.spec.unwrap().template.metadata.unwrap().labels.unwrap();
        assert_eq!(template_labels["tightbeam.dev/type"], "llm");
        assert_eq!(template_labels["tightbeam.dev/model"], "claude-sonnet");
    }

    #[test]
    fn llm_job_env_vars() {
        let job = build_llm_job(
            "claude-sonnet",
            &sample_model_spec(),
            "http://controller:9090",
            "ns",
            "s1",
        );
        let envs = job.spec.unwrap().template.spec.unwrap().containers[0]
            .env
            .as_ref()
            .unwrap()
            .clone();

        let addr = envs
            .iter()
            .find(|e| e.name == "TIGHTBEAM_CONTROLLER_ADDR")
            .unwrap();
        assert_eq!(addr.value.as_deref(), Some("http://controller:9090"));

        let model = envs
            .iter()
            .find(|e| e.name == "TIGHTBEAM_MODEL_NAME")
            .unwrap();
        assert_eq!(model.value.as_deref(), Some("claude-sonnet"));
    }

    #[test]
    fn llm_job_never_restart_and_ttl() {
        let job = build_llm_job("m", &sample_model_spec(), "http://c:9090", "ns", "s1");
        let spec = job.spec.unwrap();
        assert_eq!(spec.ttl_seconds_after_finished, Some(30));
        assert_eq!(
            spec.template.spec.unwrap().restart_policy.as_deref(),
            Some("Never")
        );
    }

    #[test]
    fn llm_job_idle_timeout_in_args() {
        let job = build_llm_job("m", &sample_model_spec(), "http://c:9090", "ns", "s1");
        let args = job.spec.unwrap().template.spec.unwrap().containers[0]
            .args
            .as_ref()
            .unwrap()
            .clone();
        assert_eq!(args, vec!["--idle-timeout", "300"]);
    }

    #[test]
    fn channel_job_has_correct_name_and_labels() {
        let job = build_channel_job(
            "discord-bot",
            &sample_channel_spec(),
            "http://controller:9090",
            "workspace-test",
            "xyz789",
        );
        assert_eq!(
            job.metadata.name.unwrap(),
            "tightbeam-channel-discord-bot-xyz789"
        );
        let labels = job.metadata.labels.unwrap();
        assert_eq!(labels["app.kubernetes.io/part-of"], "sycophant");
        assert_eq!(labels["tightbeam.dev/type"], "channel");
        assert_eq!(labels["tightbeam.dev/channel"], "discord-bot");
    }

    #[test]
    fn channel_job_restart_and_ttl() {
        let job = build_channel_job("d", &sample_channel_spec(), "http://c:9090", "ns", "s1");
        let spec = job.spec.unwrap();
        assert_eq!(spec.ttl_seconds_after_finished, Some(30));
        assert_eq!(
            spec.template.spec.unwrap().restart_policy.as_deref(),
            Some("OnFailure")
        );
    }

    #[test]
    fn channel_job_mounts_channel_secret() {
        let job = build_channel_job("d", &sample_channel_spec(), "http://c:9090", "ns", "s1");
        let pod_spec = job.spec.unwrap().template.spec.unwrap();
        let volume = &pod_spec.volumes.unwrap()[0];
        assert_eq!(volume.name, "channel-secrets");
        assert_eq!(
            volume.secret.as_ref().unwrap().secret_name.as_deref(),
            Some("discord-bot-token")
        );
        let mount = &pod_spec.containers[0].volume_mounts.as_ref().unwrap()[0];
        assert_eq!(mount.name, "channel-secrets");
        assert_eq!(mount.mount_path, "/run/secrets/channel");
    }

    #[test]
    fn channel_job_pod_template_has_labels() {
        let job = build_channel_job(
            "discord",
            &sample_channel_spec(),
            "http://c:9090",
            "ns",
            "s1",
        );
        let template_labels = job.spec.unwrap().template.metadata.unwrap().labels.unwrap();
        assert_eq!(template_labels["tightbeam.dev/type"], "channel");
        assert_eq!(template_labels["tightbeam.dev/channel"], "discord");
    }

    #[test]
    fn no_api_key_in_job_spec() {
        let job = build_llm_job("m", &sample_model_spec(), "http://c:9090", "ns", "s1");
        let json = serde_json::to_string(&job).unwrap();
        assert!(
            !json.contains("sk-ant"),
            "API key must never appear in Job spec"
        );
        assert!(
            !json.contains("api-key"),
            "API key field name must not appear in env vars"
        );
    }
}

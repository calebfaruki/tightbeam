use std::sync::Arc;

use futures::{StreamExt, TryStreamExt};
use kube::runtime::watcher::{self, Event};
use kube::{Api, Client};

use crate::crd::TightbeamModel;
use crate::state::ControllerState;

pub async fn watch_models(
    client: Client,
    namespace: &str,
    state: Arc<ControllerState>,
) -> Result<(), String> {
    let api: Api<TightbeamModel> = Api::namespaced(client, namespace);
    let mut stream = watcher::watcher(api, watcher::Config::default()).boxed();

    while let Some(event) = stream
        .try_next()
        .await
        .map_err(|e| format!("watcher error: {e}"))?
    {
        match event {
            Event::Apply(model) => {
                let name = model.metadata.name.clone().unwrap_or_default();
                tracing::info!(model = %name, "model applied");
                state.set_model_spec(model.spec).await;
            }
            Event::Delete(model) => {
                let name = model.metadata.name.clone().unwrap_or_default();
                tracing::info!(model = %name, "model deleted");
                state.clear_model_spec().await;
            }
            Event::Init => {
                tracing::info!("model watcher initialized");
                state.clear_model_spec().await;
            }
            Event::InitApply(model) => {
                let name = model.metadata.name.clone().unwrap_or_default();
                tracing::info!(model = %name, "model discovered");
                state.set_model_spec(model.spec).await;
            }
            Event::InitDone => {
                tracing::info!("model watcher initial sync complete");
            }
        }
    }

    tracing::warn!("model watcher stream ended");
    Ok(())
}

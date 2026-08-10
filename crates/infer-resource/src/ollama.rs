//! Ollama's native model-control protocol.
//!
//! Text execution remains on the shared Responses adapter. This module is
//! limited to native discovery and lifecycle control, whose payload and
//! failure semantics are distinct from a user inference request.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;

use crate::{NativeControlError, NativeInventory, NativeModelController, NativeRunningModel};

#[derive(Debug, Error)]
pub enum OllamaControlError {
    #[error("Ollama control transport failed")]
    Transport(#[from] reqwest::Error),
    #[error("Ollama control response was malformed")]
    Malformed(#[from] serde_json::Error),
}

#[derive(Debug)]
pub(super) struct OllamaInventory {
    pub installed: BTreeSet<String>,
    pub running: BTreeMap<String, OllamaRunningModel>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct OllamaRunningModel {
    pub name: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub size_vram: u64,
}

pub(super) async fn discover(
    client: &Client,
    endpoint: &str,
) -> Result<OllamaInventory, OllamaControlError> {
    let base = endpoint.trim_end_matches('/');
    let tags = client
        .get(format!("{base}/api/tags"))
        .send()
        .await?
        .error_for_status()?;
    let running = client
        .get(format!("{base}/api/ps"))
        .send()
        .await?
        .error_for_status()?;
    Ok(OllamaInventory {
        installed: parse_tags(&tags.text().await?)?,
        running: parse_running(&running.text().await?)?,
    })
}

pub(super) struct OllamaController {
    endpoint: String,
    client: Client,
}

impl OllamaController {
    pub(super) fn new(endpoint: String) -> Arc<Self> {
        Arc::new(Self {
            endpoint,
            client: Client::new(),
        })
    }
}

#[async_trait]
impl NativeModelController for OllamaController {
    async fn discover(&self) -> Result<NativeInventory, NativeControlError> {
        let inventory = discover(&self.client, &self.endpoint)
            .await
            .map_err(|error| NativeControlError::Operation(error.to_string()))?;
        Ok(NativeInventory {
            installed: inventory.installed,
            running: inventory
                .running
                .into_iter()
                .map(|(name, model)| {
                    (
                        name,
                        NativeRunningModel {
                            resident_memory_bytes: (model.size != 0).then_some(model.size),
                            resident_accelerator_memory_bytes: (model.size_vram != 0)
                                .then_some(model.size_vram),
                        },
                    )
                })
                .collect(),
        })
    }

    async fn load(&self, model: &str) -> Result<(), NativeControlError> {
        load(&self.client, &self.endpoint, model)
            .await
            .map_err(|error| NativeControlError::Operation(error.to_string()))
    }

    async fn unload(&self, model: &str) -> Result<(), NativeControlError> {
        unload(&self.client, &self.endpoint, model)
            .await
            .map_err(|error| NativeControlError::Operation(error.to_string()))
    }
}

/// Loads a model without submitting a user prompt. Ollama documents a negative
/// keep-alive value as “keep loaded”; `stream: false` gives this control call a
/// bounded response body.
pub(super) async fn load(
    client: &Client,
    endpoint: &str,
    model: &str,
) -> Result<(), OllamaControlError> {
    lifecycle_request(client, endpoint, model, -1).await
}

/// Releases a model from memory without removing its installed artifact.
pub(super) async fn unload(
    client: &Client,
    endpoint: &str,
    model: &str,
) -> Result<(), OllamaControlError> {
    lifecycle_request(client, endpoint, model, 0).await
}

async fn lifecycle_request(
    client: &Client,
    endpoint: &str,
    model: &str,
    keep_alive: i8,
) -> Result<(), OllamaControlError> {
    client
        .post(format!("{}/api/generate", endpoint.trim_end_matches('/')))
        .json(&lifecycle_payload(model, keep_alive))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

fn lifecycle_payload(model: &str, keep_alive: i8) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "keep_alive": keep_alive,
        "stream": false,
    })
}

#[derive(Debug, Deserialize)]
struct OllamaTagsResponse {
    #[serde(default)]
    models: Vec<OllamaModel>,
}

#[derive(Debug, Deserialize)]
struct OllamaModel {
    name: String,
}

#[derive(Debug, Deserialize)]
struct OllamaPsResponse {
    #[serde(default)]
    models: Vec<OllamaRunningModel>,
}

pub(super) fn parse_tags(body: &str) -> Result<BTreeSet<String>, serde_json::Error> {
    Ok(serde_json::from_str::<OllamaTagsResponse>(body)?
        .models
        .into_iter()
        .map(|model| model.name)
        .collect())
}

pub(super) fn parse_running(
    body: &str,
) -> Result<BTreeMap<String, OllamaRunningModel>, serde_json::Error> {
    Ok(serde_json::from_str::<OllamaPsResponse>(body)?
        .models
        .into_iter()
        .map(|model| (model.name.clone(), model))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_payloads_are_prompt_free_and_use_explicit_keep_alive() {
        let load = lifecycle_payload("qwen:2b", -1);
        let unload = lifecycle_payload("qwen:2b", 0);
        assert_eq!(load["keep_alive"], -1);
        assert_eq!(unload["keep_alive"], 0);
        assert_eq!(load["stream"], false);
        assert!(load.get("prompt").is_none());
        assert!(unload.get("prompt").is_none());
    }
}

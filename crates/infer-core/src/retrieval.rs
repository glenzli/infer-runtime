//! Typed text retrieval contracts.
//!
//! These payloads never become generic Job metadata or durable payloads. The
//! caller owns source freshness and index persistence.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{ContractError, Fallback, PlacementScope, RequestConstraints};

pub const MAX_RETRIEVAL_ITEMS: usize = 64;
pub const MAX_RETRIEVAL_TEXT_BYTES: usize = 32 * 1024;
pub const MAX_RETRIEVAL_TOTAL_TEXT_BYTES: usize = 512 * 1024;
pub const MAX_RETRIEVAL_ID_BYTES: usize = 128;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrievalEmbeddingRequest {
    pub model: String,
    pub inputs: Vec<RetrievalTextInput>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrievalTextInput {
    pub id: String,
    pub text: String,
    pub source_revision: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrievalRerankRequest {
    pub model: String,
    pub query: RetrievalTextInput,
    pub candidates: Vec<RetrievalTextInput>,
    pub top_n: Option<usize>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl RetrievalEmbeddingRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_model(&self.model)?;
        validate_items(&self.inputs, "inputs")?;
        validate_local_constraints(&self.metadata, "text embedding")
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        RequestConstraints::from_metadata(&self.metadata)
    }
}

impl RetrievalRerankRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_model(&self.model)?;
        validate_item(&self.query, "query")?;
        validate_items(&self.candidates, "candidates")?;
        if self
            .top_n
            .is_some_and(|top_n| top_n == 0 || top_n > self.candidates.len())
        {
            return Err(invalid(
                "top_n must be between 1 and the number of candidates",
            ));
        }
        let mut ids = BTreeSet::new();
        if self
            .candidates
            .iter()
            .any(|candidate| !ids.insert(candidate.id.as_str()))
        {
            return Err(invalid("candidate ids must be unique"));
        }
        validate_local_constraints(&self.metadata, "text reranking")
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        RequestConstraints::from_metadata(&self.metadata)
    }
}

fn validate_model(model: &str) -> Result<(), ContractError> {
    if model.trim().is_empty() {
        return Err(ContractError::MissingModel);
    }
    Ok(())
}

fn validate_items(items: &[RetrievalTextInput], field: &str) -> Result<(), ContractError> {
    if items.is_empty() || items.len() > MAX_RETRIEVAL_ITEMS {
        return Err(invalid(format!(
            "{field} must contain between 1 and {MAX_RETRIEVAL_ITEMS} items"
        )));
    }
    for item in items {
        validate_item(item, field)?;
    }
    let total = items.iter().map(|item| item.text.len()).sum::<usize>();
    if total > MAX_RETRIEVAL_TOTAL_TEXT_BYTES {
        return Err(invalid(format!(
            "{field} text exceeds {MAX_RETRIEVAL_TOTAL_TEXT_BYTES} UTF-8 bytes"
        )));
    }
    Ok(())
}

fn validate_item(item: &RetrievalTextInput, field: &str) -> Result<(), ContractError> {
    if item.id.trim().is_empty() || item.id.len() > MAX_RETRIEVAL_ID_BYTES {
        return Err(invalid(format!(
            "{field} id is required and must not exceed {MAX_RETRIEVAL_ID_BYTES} UTF-8 bytes"
        )));
    }
    if item.text.trim().is_empty() || item.text.len() > MAX_RETRIEVAL_TEXT_BYTES {
        return Err(invalid(format!(
            "{field} text must contain between 1 and {MAX_RETRIEVAL_TEXT_BYTES} UTF-8 bytes"
        )));
    }
    if item.source_revision.trim().is_empty() || item.source_revision.len() > 256 {
        return Err(invalid(format!(
            "{field} source_revision is required and must not exceed 256 UTF-8 bytes"
        )));
    }
    Ok(())
}

fn validate_local_constraints(
    metadata: &BTreeMap<String, String>,
    operation: &str,
) -> Result<(), ContractError> {
    let constraints = RequestConstraints::from_metadata(metadata)?;
    if constraints.placement != Some(PlacementScope::LocalOnly) {
        return Err(invalid(format!(
            "{operation} requires infer.placement=local_only"
        )));
    }
    if constraints.offline_required != Some(true) {
        return Err(invalid(format!(
            "{operation} requires infer.offline_required=true"
        )));
    }
    if constraints.fallback != Some(Fallback::None) {
        return Err(invalid(format!("{operation} requires infer.fallback=none")));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> ContractError {
    ContractError::InvalidVision(format!("invalid retrieval request: {}", message.into()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetrievalEmbeddingResponse {
    pub id: String,
    pub object: String,
    pub created_at: u64,
    pub status: String,
    pub data: Vec<RetrievalEmbeddingItem>,
    pub provenance: RetrievalProvenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetrievalEmbeddingItem {
    pub id: String,
    pub source_revision: String,
    pub embedding: RetrievalEmbeddingVector,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetrievalEmbeddingVector {
    pub values: Vec<f32>,
    pub dimensions: usize,
    pub normalized: bool,
    pub distance_metric: String,
    pub space: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetrievalRerankResponse {
    pub id: String,
    pub object: String,
    pub created_at: u64,
    pub status: String,
    pub query_revision: String,
    pub results: Vec<RetrievalRerankResult>,
    pub score_semantics: String,
    pub provenance: RetrievalProvenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetrievalRerankResult {
    pub candidate_id: String,
    pub source_revision: String,
    pub score: f32,
    pub rank: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetrievalProvenance {
    pub job_id: String,
    pub provider: String,
    pub deployment: String,
    pub model_build: String,
    pub model_revision: String,
    pub artifact_sha256: String,
    pub tokenizer_identity: String,
    pub instruction_revision: Option<String>,
    pub runtime: String,
    pub precision: String,
    pub embedding_space: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("infer.placement".into(), "local_only".into()),
            ("infer.offline_required".into(), "true".into()),
            ("infer.fallback".into(), "none".into()),
        ])
    }

    fn item(id: &str) -> RetrievalTextInput {
        RetrievalTextInput {
            id: id.into(),
            text: "bounded text".into(),
            source_revision: "source:1".into(),
        }
    }

    #[test]
    fn embedding_is_bounded_and_local_only() {
        let request = RetrievalEmbeddingRequest {
            model: "semantic.embed_query".into(),
            inputs: vec![item("a")],
            metadata: metadata(),
        };
        request.validate().unwrap();

        let mut remote = request.clone();
        remote
            .metadata
            .insert("infer.placement".into(), "anywhere".into());
        assert!(remote.validate().is_err());
    }

    #[test]
    fn rerank_rejects_duplicate_candidate_identity() {
        let request = RetrievalRerankRequest {
            model: "semantic.rerank".into(),
            query: item("query"),
            candidates: vec![item("same"), item("same")],
            top_n: None,
            metadata: metadata(),
        };
        assert!(request.validate().is_err());
    }
}

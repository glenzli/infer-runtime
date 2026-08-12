use std::collections::BTreeMap;

use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::{Client, Result};

pub const TEXT_EMBEDDING_CAPABILITIES: &[&str] = &["infer.text.embedding@20260812.1"];
pub const TEXT_RERANK_CAPABILITIES: &[&str] = &["infer.text.rerank@20260812.1"];

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetrievalTextInput {
    pub id: String,
    pub text: String,
    pub source_revision: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetrievalEmbeddingRequest {
    pub model: String,
    pub inputs: Vec<RetrievalTextInput>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetrievalRerankRequest {
    pub model: String,
    pub query: RetrievalTextInput,
    pub candidates: Vec<RetrievalTextInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_n: Option<usize>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RetrievalEmbeddingResponse {
    pub id: String,
    pub object: String,
    pub created_at: u64,
    pub status: String,
    pub data: Vec<RetrievalEmbeddingItem>,
    pub provenance: RetrievalProvenance,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RetrievalEmbeddingItem {
    pub id: String,
    pub source_revision: String,
    pub embedding: RetrievalEmbeddingVector,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RetrievalEmbeddingVector {
    pub values: Vec<f32>,
    pub dimensions: usize,
    pub normalized: bool,
    pub distance_metric: String,
    pub space: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
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

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RetrievalRerankResult {
    pub candidate_id: String,
    pub source_revision: String,
    pub score: f32,
    pub rank: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
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

impl Client {
    pub async fn embed_queries(
        &self,
        request: &RetrievalEmbeddingRequest,
    ) -> Result<RetrievalEmbeddingResponse> {
        self.send_capability_json(
            TEXT_EMBEDDING_CAPABILITIES,
            Method::POST,
            "/infer/v1/text/query-embeddings",
            Some(request),
        )
        .await
    }

    pub async fn embed_documents(
        &self,
        request: &RetrievalEmbeddingRequest,
    ) -> Result<RetrievalEmbeddingResponse> {
        self.send_capability_json(
            TEXT_EMBEDDING_CAPABILITIES,
            Method::POST,
            "/infer/v1/text/document-embeddings",
            Some(request),
        )
        .await
    }

    pub async fn rerank(
        &self,
        request: &RetrievalRerankRequest,
    ) -> Result<RetrievalRerankResponse> {
        self.send_capability_json(
            TEXT_RERANK_CAPABILITIES,
            Method::POST,
            "/infer/v1/text/rerank",
            Some(request),
        )
        .await
    }
}

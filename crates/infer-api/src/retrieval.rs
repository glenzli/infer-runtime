//! Typed, strict JSON transport for local semantic retrieval.

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, Response, StatusCode},
};
use infer_core::{RetrievalEmbeddingRequest, RetrievalRerankRequest};
use infer_provider::RetrievalEmbeddingKind;

use crate::{ApiError, ApiState, authenticate, response, strict_json};

pub(super) async fn create_query_embeddings(
    State(state): State<ApiState>,
    headers: HeaderMap,
    request: Result<Json<RetrievalEmbeddingRequest>, JsonRejection>,
) -> Result<Response<axum::body::Body>, ApiError> {
    create_embeddings(state, headers, request, RetrievalEmbeddingKind::Query).await
}

pub(super) async fn create_document_embeddings(
    State(state): State<ApiState>,
    headers: HeaderMap,
    request: Result<Json<RetrievalEmbeddingRequest>, JsonRejection>,
) -> Result<Response<axum::body::Body>, ApiError> {
    create_embeddings(state, headers, request, RetrievalEmbeddingKind::Documents).await
}

async fn create_embeddings(
    state: ApiState,
    headers: HeaderMap,
    request: Result<Json<RetrievalEmbeddingRequest>, JsonRejection>,
    kind: RetrievalEmbeddingKind,
) -> Result<Response<axum::body::Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let mut request = strict_json(request)?;
    request.metadata = super::vision::fail_closed_metadata(request.metadata, "text retrieval")?;
    let result = state
        .runtime
        .execute_retrieval_embedding(&app_id, request, kind)
        .await?;
    response(
        StatusCode::OK,
        "application/json",
        serde_json::to_vec(&result).expect("retrieval embedding response is serializable"),
    )
}

pub(super) async fn create_rerank(
    State(state): State<ApiState>,
    headers: HeaderMap,
    request: Result<Json<RetrievalRerankRequest>, JsonRejection>,
) -> Result<Response<axum::body::Body>, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    let mut request = strict_json(request)?;
    request.metadata = super::vision::fail_closed_metadata(request.metadata, "text reranking")?;
    let result = state
        .runtime
        .execute_retrieval_rerank(&app_id, request)
        .await?;
    response(
        StatusCode::OK,
        "application/json",
        serde_json::to_vec(&result).expect("retrieval rerank response is serializable"),
    )
}

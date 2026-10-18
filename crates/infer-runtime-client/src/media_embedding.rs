//! Optional unified media retrieval; vectors never mix with other model spaces.

use crate::{Client, Error, Result};
use reqwest::{
    Method,
    multipart::{Form, Part},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

pub const MEDIA_EMBEDDING_CAPABILITIES: &[&str] = &["infer.media.embedding@20261013.1"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaQueryEmbeddingRequest {
    pub model: String,
    pub text: String,
    pub query_revision: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MediaEmbeddingKind {
    Text,
    Image,
    Audio,
    Video,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MediaEmbeddingSpace {
    pub identity: String,
    pub dimensions: usize,
    pub normalized: bool,
    pub distance_metric: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MediaEmbeddingResponse {
    pub id: String,
    pub object: String,
    pub created_at: u64,
    pub model: String,
    pub input_kind: MediaEmbeddingKind,
    pub input_revision: String,
    pub embedding: Vec<f32>,
    pub embedding_space: MediaEmbeddingSpace,
    pub processing: MediaEmbeddingProcessing,
    pub provenance: MediaEmbeddingProvenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MediaEmbeddingProcessing {
    pub input_tokens: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decoded_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_frames: Option<usize>,
    pub video_audio_included: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MediaEmbeddingProvenance {
    pub job_id: String,
    pub provider: String,
    pub deployment: String,
    pub model_build: String,
    pub model_revision: String,
    pub artifact_set_sha256: String,
    pub runtime: String,
    pub precision: String,
    pub preprocessing_identity: String,
    pub postprocessing_identity: String,
    pub tokenizer_identity: String,
    pub instruction_revision: String,
}

impl Client {
    pub async fn embed_media_query(
        &self,
        request: &MediaQueryEmbeddingRequest,
    ) -> Result<MediaEmbeddingResponse> {
        let result = self
            .send_capability_json(
                MEDIA_EMBEDDING_CAPABILITIES,
                Method::POST,
                "/infer/v1/media/query-embeddings",
                Some(request),
            )
            .await?;
        validate_response(
            &result,
            &request.model,
            &request.query_revision,
            MediaEmbeddingKind::Text,
        )?;
        Ok(result)
    }

    pub async fn embed_media_image(
        &self,
        image: &Path,
        content_type: &'static str,
        source_revision: &str,
        metadata: &BTreeMap<String, String>,
    ) -> Result<MediaEmbeddingResponse> {
        self.media_file(
            "/infer/v1/media/image-embeddings",
            "semantic.media_image",
            image,
            content_type,
            source_revision,
            true,
            metadata,
        )
        .await
    }

    pub async fn embed_media_audio(
        &self,
        audio: &Path,
        content_type: &'static str,
        source_revision: &str,
        metadata: &BTreeMap<String, String>,
    ) -> Result<MediaEmbeddingResponse> {
        self.media_file(
            "/infer/v1/media/audio-embeddings",
            "semantic.media_audio",
            audio,
            content_type,
            source_revision,
            false,
            metadata,
        )
        .await
    }

    /// Visual frames only. Send an audio segment separately to embed its sound.
    pub async fn embed_media_video(
        &self,
        video: &Path,
        content_type: &'static str,
        source_revision: &str,
        metadata: &BTreeMap<String, String>,
    ) -> Result<MediaEmbeddingResponse> {
        self.media_file(
            "/infer/v1/media/video-embeddings",
            "semantic.media_video",
            video,
            content_type,
            source_revision,
            false,
            metadata,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn media_file(
        &self,
        route: &'static str,
        model: &'static str,
        file: &Path,
        content_type: &'static str,
        revision: &str,
        image: bool,
        metadata: &BTreeMap<String, String>,
    ) -> Result<MediaEmbeddingResponse> {
        let kind = match route {
            "/infer/v1/media/image-embeddings" => MediaEmbeddingKind::Image,
            "/infer/v1/media/audio-embeddings" => MediaEmbeddingKind::Audio,
            _ => MediaEmbeddingKind::Video,
        };
        let content_type_allowed = match kind {
            MediaEmbeddingKind::Image => matches!(content_type, "image/jpeg" | "image/png"),
            MediaEmbeddingKind::Audio => matches!(
                content_type,
                "audio/wav"
                    | "audio/x-wav"
                    | "audio/flac"
                    | "audio/mpeg"
                    | "audio/ogg"
                    | "audio/mp4"
            ),
            MediaEmbeddingKind::Video => {
                matches!(content_type, "video/mp4" | "video/quicktime" | "video/webm")
            }
            MediaEmbeddingKind::Text => false,
        };
        if !content_type_allowed {
            return Err(Error::Input("unsupported media content type".into()));
        }
        let bytes = crate::transport::read_bounded_file(file, 25 * 1024 * 1024, "media").await?;
        let metadata = serde_json::to_string(metadata)
            .map_err(|error| Error::MalformedResponse(error.to_string()))?;
        let revision = revision.to_owned();
        let expected_revision = revision.clone();
        let response = self
            .send_capability_with(MEDIA_EMBEDDING_CAPABILITIES, move |http, endpoint| {
                let mut form = Form::new()
                    .part(
                        "file",
                        Part::bytes(bytes.clone())
                            .file_name("media.bin")
                            .mime_str(content_type)
                            .expect("validated media MIME type"),
                    )
                    .text("model", model)
                    .text("source_revision", revision.clone())
                    .text("metadata", metadata.clone());
                if image {
                    form = form.text("image_orientation", "display_pixels_orientation_normalized");
                }
                http.post(format!("{endpoint}{route}")).multipart(form)
            })
            .await?;
        let result = crate::transport::decode(response).await?;
        validate_response(&result, model, &expected_revision, kind)?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invalid_or_mismatched_mime_is_rejected_before_file_access_or_network() {
        let client = Client::builder().build().unwrap();
        let metadata = BTreeMap::new();
        for kind in [
            MediaEmbeddingKind::Image,
            MediaEmbeddingKind::Audio,
            MediaEmbeddingKind::Video,
        ] {
            let path = Path::new("/absent/private-media-input");
            let result = match kind {
                MediaEmbeddingKind::Image => {
                    client
                        .embed_media_image(path, "invalid mime", "r1", &metadata)
                        .await
                }
                MediaEmbeddingKind::Audio => {
                    client
                        .embed_media_audio(path, "image/png", "r1", &metadata)
                        .await
                }
                MediaEmbeddingKind::Video => {
                    client
                        .embed_media_video(path, "audio/wav", "r1", &metadata)
                        .await
                }
                MediaEmbeddingKind::Text => unreachable!(),
            };
            assert!(
                matches!(result, Err(Error::Input(message)) if message == "unsupported media content type")
            );
        }
    }
}

fn validate_response(
    result: &MediaEmbeddingResponse,
    model: &str,
    revision: &str,
    kind: MediaEmbeddingKind,
) -> Result<()> {
    let norm = result
        .embedding
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    let processing = &result.processing;
    let duration_valid = |bound: f64| {
        processing
            .decoded_seconds
            .is_some_and(|seconds| seconds.is_finite() && seconds > 0.0 && seconds <= bound + 1e-6)
    };
    let valid_processing = match kind {
        MediaEmbeddingKind::Text | MediaEmbeddingKind::Image => {
            processing.decoded_seconds.is_none() && processing.video_frames.is_none()
        }
        MediaEmbeddingKind::Audio => duration_valid(30.0) && processing.video_frames.is_none(),
        MediaEmbeddingKind::Video => {
            duration_valid(16.0)
                && processing
                    .video_frames
                    .is_some_and(|frames| (1..=16).contains(&frames))
        }
    };
    if result.object != "media.embedding"
        || result.model != model
        || result.input_revision != revision
        || result.input_kind != kind
        || result.embedding_space.dimensions != 768
        || !result.embedding_space.normalized
        || result.embedding_space.distance_metric != "cosine"
        || result.embedding_space.identity.is_empty()
        || result.embedding.len() != 768
        || result.embedding.iter().any(|value| !value.is_finite())
        || !norm.is_finite()
        || (norm - 1.0).abs() > 1e-4
        || !(1..=8192).contains(&processing.input_tokens)
        || processing.video_audio_included
        || !valid_processing
        || result.provenance.job_id != result.id
        || result.provenance.model_build.is_empty()
        || result.provenance.artifact_set_sha256.len() != 64
    {
        return Err(Error::MalformedResponse(
            "invalid media embedding response".into(),
        ));
    }
    Ok(())
}

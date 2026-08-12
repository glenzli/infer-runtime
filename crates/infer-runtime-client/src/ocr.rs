use std::{collections::BTreeMap, path::Path};

use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};

use crate::{Client, Error, ImageGeometry, Point, Result, transport::decode};

pub const DOCUMENT_OCR_CAPABILITIES: &[&str] = &["infer.document.ocr@20260812.1"];

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct DocumentOcrResponse {
    pub id: String,
    pub object: String,
    pub created_at: u64,
    pub status: String,
    pub source_revision: String,
    pub image: ImageGeometry,
    pub lines: Vec<OcrTextLine>,
    pub provenance: OcrProvenance,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct OcrTextLine {
    pub polygon: [Point; 4],
    pub text: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct OcrProvenance {
    pub job_id: String,
    pub provider: String,
    pub deployment: String,
    pub model_build: String,
    pub detection_revision: String,
    pub detection_artifact_sha256: String,
    pub recognition_revision: String,
    pub recognition_artifact_sha256: String,
    pub preprocessing_identity: String,
    pub postprocessing_identity: String,
    pub runtime: String,
    pub requested_execution_provider: String,
    pub actual_execution_provider: String,
    pub precision: String,
}

impl Client {
    pub async fn ocr_document(
        &self,
        image: &Path,
        content_type: &'static str,
        source_revision: &str,
        metadata: &BTreeMap<String, String>,
    ) -> Result<DocumentOcrResponse> {
        let bytes = crate::transport::read_bounded_file(
            image,
            crate::transport::MAX_IMAGE_INPUT_BYTES,
            "image",
        )
        .await?;
        let filename = image
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image.bin")
            .to_owned();
        let metadata = serde_json::to_string(metadata)
            .map_err(|error| Error::MalformedResponse(error.to_string()))?;
        let source_revision = source_revision.to_owned();
        let response = self
            .send_capability_with(DOCUMENT_OCR_CAPABILITIES, move |http, endpoint| {
                let image = Part::bytes(bytes.clone())
                    .file_name(filename.clone())
                    .mime_str(content_type)
                    .expect("static MIME type is valid");
                http.post(format!("{endpoint}/infer/v1/documents/ocr"))
                    .multipart(
                        Form::new()
                            .text("model", "document.ocr")
                            .text("source_revision", source_revision.clone())
                            .text("image_orientation", "display_pixels_orientation_normalized")
                            .text("metadata", metadata.clone())
                            .part("image", image),
                    )
            })
            .await?;
        decode(response).await
    }
}

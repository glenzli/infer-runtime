//! Responses image/text input translation for the Codex App Server bridge.

use std::path::Path;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use tokio::fs;
use uuid::Uuid;

use crate::ProviderError;

const MAX_IMAGES: usize = 8;
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_TOTAL_IMAGE_BYTES: usize = 40 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct PreparedTurnInput {
    pub items: Vec<Value>,
    pub has_images: bool,
}

pub(super) async fn prepare_turn_input(
    input: &Value,
    workspace: &Path,
) -> Result<PreparedTurnInput, ProviderError> {
    let mut builder = InputBuilder {
        workspace,
        items: Vec::new(),
        image_count: 0,
        image_bytes: 0,
    };
    if let Some(text) = input.as_str() {
        builder.push_text(text.to_owned())?;
    } else {
        let items = input.as_array().ok_or_else(|| {
            ProviderError::InvalidInput(
                "Codex bridge input must be text or a Responses input array".into(),
            )
        })?;
        for item in items {
            builder.push_item(item).await?;
        }
    }
    if builder.items.is_empty() {
        return Err(ProviderError::InvalidInput(
            "Codex bridge input cannot be empty".into(),
        ));
    }
    Ok(PreparedTurnInput {
        items: builder.items,
        has_images: builder.image_count > 0,
    })
}

struct InputBuilder<'a> {
    workspace: &'a Path,
    items: Vec<Value>,
    image_count: usize,
    image_bytes: usize,
}

impl InputBuilder<'_> {
    async fn push_item(&mut self, item: &Value) -> Result<(), ProviderError> {
        if matches!(
            item.get("type").and_then(Value::as_str),
            Some("input_image" | "input_text")
        ) {
            return self.push_part(item).await;
        }
        let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
        if !matches!(role, "user" | "assistant" | "developer" | "system") {
            return Err(ProviderError::InvalidInput(
                "Codex bridge message role is unsupported".into(),
            ));
        }
        let content = item.get("content").ok_or_else(|| {
            ProviderError::InvalidInput("Responses message omitted content".into())
        })?;
        if let Some(text) = content.as_str() {
            return self.push_text(format!("{role}: {text}"));
        }
        let parts = content.as_array().ok_or_else(|| {
            ProviderError::InvalidInput("Responses message content must be text or parts".into())
        })?;
        self.push_text(format!("{role}:"))?;
        for part in parts {
            self.push_part(part).await?;
        }
        Ok(())
    }

    async fn push_part(&mut self, part: &Value) -> Result<(), ProviderError> {
        match part.get("type").and_then(Value::as_str) {
            Some("input_text" | "text") => {
                let text = part.get("text").and_then(Value::as_str).ok_or_else(|| {
                    ProviderError::InvalidInput("text input part omitted text".into())
                })?;
                self.push_text(text.to_owned())
            }
            Some("input_image") => self.push_image(part).await,
            Some(other) => Err(ProviderError::InvalidInput(format!(
                "Codex bridge does not accept input part type {other}"
            ))),
            None => Err(ProviderError::InvalidInput(
                "Responses input part omitted type".into(),
            )),
        }
    }

    fn push_text(&mut self, text: String) -> Result<(), ProviderError> {
        if text.trim().is_empty() {
            return Err(ProviderError::InvalidInput(
                "text input part cannot be empty".into(),
            ));
        }
        self.items
            .push(json!({"type": "text", "text": text, "text_elements": []}));
        Ok(())
    }

    async fn push_image(&mut self, part: &Value) -> Result<(), ProviderError> {
        if self.image_count >= MAX_IMAGES {
            return Err(ProviderError::InvalidInput(format!(
                "Codex bridge accepts at most {MAX_IMAGES} images"
            )));
        }
        let image_url = part
            .get("image_url")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::InvalidInput("image part omitted image_url".into()))?;
        if image_url.starts_with("https://") {
            self.items.push(json!({"type": "image", "url": image_url}));
            self.image_count += 1;
            return Ok(());
        }
        let (extension, bytes) = decode_image_data_url(image_url)?;
        if bytes.len() > MAX_IMAGE_BYTES
            || self.image_bytes.saturating_add(bytes.len()) > MAX_TOTAL_IMAGE_BYTES
        {
            return Err(ProviderError::InvalidInput(
                "Codex bridge image payload exceeds its bounded limit".into(),
            ));
        }
        image::load_from_memory(&bytes).map_err(|_| {
            ProviderError::InvalidInput("image payload is not valid JPEG/PNG".into())
        })?;
        let path = self
            .workspace
            .join(format!("input-{}.{}", Uuid::new_v4().simple(), extension));
        fs::write(&path, &bytes).await?;
        self.items.push(json!({
            "type": "localImage",
            "path": path.to_string_lossy(),
        }));
        self.image_count += 1;
        self.image_bytes += bytes.len();
        Ok(())
    }
}

fn decode_image_data_url(value: &str) -> Result<(&'static str, Vec<u8>), ProviderError> {
    let (header, encoded) = value.split_once(',').ok_or_else(|| {
        ProviderError::InvalidInput("image_url must be HTTPS or a JPEG/PNG data URL".into())
    })?;
    let extension = match header {
        "data:image/jpeg;base64" | "data:image/jpg;base64" => "jpg",
        "data:image/png;base64" => "png",
        _ => {
            return Err(ProviderError::InvalidInput(
                "Codex bridge accepts only base64 JPEG/PNG data URLs".into(),
            ));
        }
    };
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| ProviderError::InvalidInput("image data URL is invalid base64".into()))?;
    Ok((extension, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn preserves_text_and_stages_bounded_local_images() {
        let workspace = tempfile::tempdir().unwrap();
        let image = image::DynamicImage::new_rgb8(1, 1);
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        let encoded = STANDARD.encode(bytes.into_inner());
        let prepared = prepare_turn_input(
            &json!([{"role":"user","content":[
                {"type":"input_text","text":"describe"},
                {"type":"input_image","image_url":format!("data:image/png;base64,{encoded}")}
            ]}]),
            workspace.path(),
        )
        .await
        .unwrap();
        assert!(prepared.has_images);
        assert_eq!(prepared.items.len(), 3);
        let path = prepared.items[2]["path"].as_str().unwrap();
        assert!(Path::new(path).starts_with(workspace.path()));
    }

    #[tokio::test]
    async fn rejects_local_paths_and_unencrypted_remote_urls() {
        let workspace = tempfile::tempdir().unwrap();
        for image_url in ["/tmp/secret.png", "http://example.test/image.png"] {
            let error = prepare_turn_input(
                &json!([{"type":"input_image","image_url":image_url}]),
                workspace.path(),
            )
            .await
            .unwrap_err();
            assert!(matches!(error, ProviderError::InvalidInput(_)));
        }
    }
}

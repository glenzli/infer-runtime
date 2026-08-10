use image::imageops::FilterType;
use infer_artifact::ArtifactStore;
use infer_core::{
    ImageEmbeddingRequest, ImageGeometry, OnnxAdapterKind, OnnxModelBuildConfig,
    SemanticEmbeddingVector, TextEmbeddingRequest,
};
use ort::{session::RunOptions, value::Tensor};
use tokenizers::{
    PaddingDirection, PaddingParams, PaddingStrategy, Tokenizer, TruncationDirection,
    TruncationParams, TruncationStrategy,
};

use super::{
    ImageEmbeddingExecutionOutput, SessionEntry, TextEmbeddingExecutionOutput, decode_image,
    execution_provenance, tensor_data,
};
use crate::ProviderError;

const IMAGE_SIZE: u32 = 224;

pub(super) fn load_tokenizer(
    store: &ArtifactStore,
    build_id: &str,
    onnx: &OnnxModelBuildConfig,
) -> Result<Option<Tokenizer>, ProviderError> {
    if onnx.adapter != OnnxAdapterKind::SiglipTextEmbedding {
        return Ok(None);
    }
    let contract = onnx.text_preprocessing.as_ref().ok_or_else(|| {
        ProviderError::Protocol("SigLIP text Build has no tokenizer contract".into())
    })?;
    let path = store
        .resolve_onnx_auxiliary(build_id, &contract.tokenizer_artifact, onnx)
        .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?;
    let mut tokenizer = Tokenizer::from_file(path)
        .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?;
    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length: contract.max_length,
            strategy: TruncationStrategy::LongestFirst,
            stride: 0,
            direction: TruncationDirection::Right,
        }))
        .map_err(|error| ProviderError::Protocol(error.to_string()))?;
    tokenizer.with_padding(Some(PaddingParams {
        strategy: PaddingStrategy::Fixed(contract.max_length),
        direction: PaddingDirection::Right,
        pad_to_multiple_of: None,
        pad_id: contract.pad_token_id,
        pad_type_id: 0,
        pad_token: "<pad>".into(),
    }));
    Ok(Some(tokenizer))
}

pub(super) fn run_image(
    entry: &SessionEntry,
    request: ImageEmbeddingRequest,
    run_options: &RunOptions,
) -> Result<ImageEmbeddingExecutionOutput, ProviderError> {
    let decoded = decode_image(&request.image)?;
    let (width, height) = decoded.dimensions();
    let resized = image::imageops::resize(&decoded, IMAGE_SIZE, IMAGE_SIZE, FilterType::CatmullRom);
    let input = image_tensor(&resized)?;
    let mut session = entry.session.lock().expect("ONNX session poisoned");
    let outputs = session
        .run_with_options(ort::inputs!["pixel_values" => input], run_options)
        .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?;
    let embedding = semantic_embedding(entry, tensor_data(&outputs, "embedding")?)?;
    Ok(ImageEmbeddingExecutionOutput {
        image: ImageGeometry {
            width,
            height,
            orientation: request.image_orientation,
        },
        embedding,
        provenance: execution_provenance(entry),
    })
}

pub(super) fn run_text(
    entry: &SessionEntry,
    request: TextEmbeddingRequest,
    run_options: &RunOptions,
) -> Result<TextEmbeddingExecutionOutput, ProviderError> {
    let onnx = entry.build.onnx.as_ref().expect("validated ONNX build");
    let contract = onnx
        .text_preprocessing
        .as_ref()
        .expect("validated SigLIP tokenizer contract");
    let input_text = if contract.lowercase {
        request.text.to_lowercase()
    } else {
        request.text
    };
    let tokenizer = entry
        .tokenizer
        .as_ref()
        .ok_or_else(|| ProviderError::Protocol("SigLIP tokenizer is not loaded".into()))?;
    let encoding = tokenizer
        .encode(input_text, true)
        .map_err(|error| ProviderError::InvalidInput(error.to_string()))?;
    if encoding.len() != contract.max_length {
        return Err(ProviderError::Protocol(format!(
            "SigLIP tokenizer emitted {} ids instead of {}",
            encoding.len(),
            contract.max_length
        )));
    }
    let input_ids = encoding
        .get_ids()
        .iter()
        .map(|id| i64::from(*id))
        .collect::<Vec<_>>();
    let input = Tensor::from_array(([1_usize, contract.max_length], input_ids))
        .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?;
    let mut session = entry.session.lock().expect("ONNX session poisoned");
    let outputs = session
        .run_with_options(ort::inputs!["input_ids" => input], run_options)
        .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?;
    let embedding = semantic_embedding(entry, tensor_data(&outputs, "embedding")?)?;
    Ok(TextEmbeddingExecutionOutput {
        embedding,
        provenance: execution_provenance(entry),
    })
}

fn image_tensor(image: &image::RgbImage) -> Result<Tensor<f32>, ProviderError> {
    let plane = (IMAGE_SIZE * IMAGE_SIZE) as usize;
    let mut data = vec![0.0_f32; plane * 3];
    for (x, y, pixel) in image.enumerate_pixels() {
        let offset = y as usize * IMAGE_SIZE as usize + x as usize;
        for channel in 0..3 {
            data[channel * plane + offset] = pixel[channel] as f32 / 127.5 - 1.0;
        }
    }
    Tensor::from_array(([1_usize, 3, IMAGE_SIZE as usize, IMAGE_SIZE as usize], data))
        .map_err(|error| ProviderError::NativeRuntime(error.to_string()))
}

fn semantic_embedding(
    entry: &SessionEntry,
    raw: &[f32],
) -> Result<SemanticEmbeddingVector, ProviderError> {
    let onnx = entry.build.onnx.as_ref().expect("validated ONNX build");
    let space = onnx
        .embedding_space
        .as_ref()
        .ok_or_else(|| ProviderError::Protocol("SigLIP Build has no embedding space".into()))?;
    if raw.len() != space.dimensions || raw.iter().any(|value| !value.is_finite()) {
        return Err(ProviderError::Protocol(format!(
            "SigLIP output must contain exactly {} finite float32 values",
            space.dimensions
        )));
    }
    let norm = raw.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() || norm <= f32::EPSILON {
        return Err(ProviderError::Protocol(
            "SigLIP output cannot be L2-normalized".into(),
        ));
    }
    Ok(SemanticEmbeddingVector {
        values: raw.iter().map(|value| value / norm).collect(),
        dimensions: space.dimensions,
        normalized: true,
        distance_metric: space.distance_metric.clone(),
        space: space.identity.clone(),
    })
}

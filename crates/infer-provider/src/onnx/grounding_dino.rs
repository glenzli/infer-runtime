use image::imageops::FilterType;
use infer_artifact::ArtifactStore;
use infer_core::{
    ImageGeometry, NormalizedBoundingBox, OnnxAdapterKind, OnnxModelBuildConfig,
    SemanticGroundingRegion, SemanticGroundingRequest,
    VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS,
};
use ort::{session::RunOptions, value::Tensor};
use tokenizers::{Tokenizer, TruncationDirection, TruncationParams, TruncationStrategy};

use super::{
    SemanticGroundingExecutionOutput, SessionEntry, decode_image, execution_provenance, parameter,
    tensor_data,
};
use crate::ProviderError;

const IMAGE_EDGE: usize = 800;
const QUERY_COUNT: usize = 900;
const LOGIT_TOKEN_COUNT: usize = 256;

pub(super) fn load_tokenizer(
    store: &ArtifactStore,
    build_id: &str,
    onnx: &OnnxModelBuildConfig,
) -> Result<Option<Tokenizer>, ProviderError> {
    if onnx.adapter != OnnxAdapterKind::GroundingDinoSemanticGrounding {
        return Ok(None);
    }
    let contract = onnx.text_preprocessing.as_ref().ok_or_else(|| {
        ProviderError::Protocol("Grounding DINO Build has no tokenizer contract".into())
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
    tokenizer.with_padding(None);
    Ok(Some(tokenizer))
}

pub(super) fn run(
    entry: &SessionEntry,
    request: SemanticGroundingRequest,
    run_options: &RunOptions,
) -> Result<SemanticGroundingExecutionOutput, ProviderError> {
    validate_build_contract(entry)?;
    let decoded = decode_image(&request.image)?;
    let (width, height) = decoded.dimensions();
    let pixel_values = image_tensor(&decoded)?;
    let tokenizer = entry
        .tokenizer
        .as_ref()
        .ok_or_else(|| ProviderError::Protocol("Grounding DINO tokenizer is not loaded".into()))?;
    let mut query = request.query.trim().to_owned();
    if !query.ends_with('.') {
        query.push('.');
    }
    let encoding = tokenizer
        .encode(query, true)
        .map_err(|error| ProviderError::InvalidInput(error.to_string()))?;
    let token_count = encoding.len();
    if !(3..=LOGIT_TOKEN_COUNT).contains(&token_count) {
        return Err(ProviderError::InvalidInput(
            "semantic query produced an unsupported token count".into(),
        ));
    }
    let as_i64 = |values: &[u32]| {
        values
            .iter()
            .map(|value| i64::from(*value))
            .collect::<Vec<_>>()
    };
    let input_ids = Tensor::from_array(([1_usize, token_count], as_i64(encoding.get_ids())))
        .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?;
    let token_type_ids =
        Tensor::from_array(([1_usize, token_count], as_i64(encoding.get_type_ids())))
            .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?;
    let attention_mask = Tensor::from_array((
        [1_usize, token_count],
        as_i64(encoding.get_attention_mask()),
    ))
    .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?;
    let pixel_mask = Tensor::from_array((
        [1_usize, IMAGE_EDGE, IMAGE_EDGE],
        vec![1_i64; IMAGE_EDGE * IMAGE_EDGE],
    ))
    .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?;

    let mut session = entry.session.lock().expect("ONNX session poisoned");
    let outputs = session
        .run_with_options(
            ort::inputs![
                "pixel_values" => pixel_values,
                "input_ids" => input_ids,
                "token_type_ids" => token_type_ids,
                "attention_mask" => attention_mask,
                "pixel_mask" => pixel_mask,
            ],
            run_options,
        )
        .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?;
    let logits = tensor_data(&outputs, "logits")?;
    let boxes = tensor_data(&outputs, "pred_boxes")?;
    if logits.len() != QUERY_COUNT * LOGIT_TOKEN_COUNT || boxes.len() != QUERY_COUNT * 4 {
        return Err(ProviderError::Protocol(
            "Grounding DINO output shape does not match the typed adapter".into(),
        ));
    }
    let nms_threshold = parameter(
        entry.build.onnx.as_ref().expect("validated ONNX build"),
        "nms_threshold",
        0.8,
    )? as f32;
    let regions = decode_regions(
        logits,
        boxes,
        token_count,
        request.score_threshold,
        nms_threshold,
        usize::from(request.maximum_regions),
    );
    Ok(SemanticGroundingExecutionOutput {
        image: ImageGeometry {
            width,
            height,
            orientation: VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS.into(),
        },
        regions,
        provenance: execution_provenance(entry),
    })
}

fn image_tensor(image: &image::RgbImage) -> Result<Tensor<f32>, ProviderError> {
    let resized = image::imageops::resize(
        image,
        IMAGE_EDGE as u32,
        IMAGE_EDGE as u32,
        FilterType::Triangle,
    );
    let plane = IMAGE_EDGE * IMAGE_EDGE;
    let mut data = vec![0.0_f32; plane * 3];
    let mean = [0.485_f32, 0.456, 0.406];
    let std = [0.229_f32, 0.224, 0.225];
    for (x, y, pixel) in resized.enumerate_pixels() {
        let offset = y as usize * IMAGE_EDGE + x as usize;
        for channel in 0..3 {
            data[channel * plane + offset] =
                (pixel[channel] as f32 / 255.0 - mean[channel]) / std[channel];
        }
    }
    Tensor::from_array(([1_usize, 3, IMAGE_EDGE, IMAGE_EDGE], data))
        .map_err(|error| ProviderError::NativeRuntime(error.to_string()))
}

#[derive(Clone)]
struct CandidateRegion {
    score: f32,
    bounds: NormalizedBoundingBox,
}

fn decode_regions(
    logits: &[f32],
    boxes: &[f32],
    token_count: usize,
    score_threshold: f32,
    nms_threshold: f32,
    maximum_regions: usize,
) -> Vec<SemanticGroundingRegion> {
    let mut candidates = Vec::new();
    for query in 0..QUERY_COUNT {
        let logits = &logits[query * LOGIT_TOKEN_COUNT..(query + 1) * LOGIT_TOKEN_COUNT];
        let score = logits[1..token_count.saturating_sub(1)]
            .iter()
            .copied()
            .map(sigmoid)
            .max_by(f32::total_cmp)
            .unwrap_or(0.0);
        if score < score_threshold {
            continue;
        }
        let base = query * 4;
        let center_x = boxes[base];
        let center_y = boxes[base + 1];
        let width = boxes[base + 2];
        let height = boxes[base + 3];
        if [center_x, center_y, width, height]
            .iter()
            .any(|value| !value.is_finite())
            || width <= 0.0
            || height <= 0.0
        {
            continue;
        }
        let left = (center_x - width / 2.0).clamp(0.0, 1.0);
        let top = (center_y - height / 2.0).clamp(0.0, 1.0);
        let right = (center_x + width / 2.0).clamp(0.0, 1.0);
        let bottom = (center_y + height / 2.0).clamp(0.0, 1.0);
        if right <= left || bottom <= top {
            continue;
        }
        candidates.push(CandidateRegion {
            score,
            bounds: NormalizedBoundingBox {
                x: left,
                y: top,
                width: right - left,
                height: bottom - top,
            },
        });
    }
    candidates.sort_by(|left, right| right.score.total_cmp(&left.score));
    let mut kept = Vec::new();
    for candidate in candidates {
        if kept.iter().all(|existing: &CandidateRegion| {
            intersection_over_union(&candidate.bounds, &existing.bounds) <= nms_threshold
        }) {
            kept.push(candidate);
            if kept.len() == maximum_regions {
                break;
            }
        }
    }
    kept.into_iter()
        .enumerate()
        .map(|(index, candidate)| SemanticGroundingRegion {
            region_id: format!("region-{}", index + 1),
            score: candidate.score,
            bounding_box: candidate.bounds,
        })
        .collect()
}

fn sigmoid(value: f32) -> f32 {
    1.0 / (1.0 + (-value).exp())
}

fn intersection_over_union(left: &NormalizedBoundingBox, right: &NormalizedBoundingBox) -> f32 {
    let left_x2 = left.x + left.width;
    let left_y2 = left.y + left.height;
    let right_x2 = right.x + right.width;
    let right_y2 = right.y + right.height;
    let intersection = (left_x2.min(right_x2) - left.x.max(right.x)).max(0.0)
        * (left_y2.min(right_y2) - left.y.max(right.y)).max(0.0);
    let union = left.width * left.height + right.width * right.height - intersection;
    if union <= 0.0 {
        0.0
    } else {
        intersection / union
    }
}

fn validate_build_contract(entry: &SessionEntry) -> Result<(), ProviderError> {
    let onnx = entry.build.onnx.as_ref().ok_or_else(|| {
        ProviderError::Protocol("Grounding DINO Build has no ONNX contract".into())
    })?;
    let image = onnx.preprocessing.as_ref().ok_or_else(|| {
        ProviderError::Protocol("Grounding DINO Build has no image preprocessing contract".into())
    })?;
    let text = onnx.text_preprocessing.as_ref().ok_or_else(|| {
        ProviderError::Protocol("Grounding DINO Build has no text preprocessing contract".into())
    })?;
    let tensor = |name: &str, dtype: &str, shape: &[&str]| infer_core::TensorContractConfig {
        name: name.into(),
        dtype: dtype.into(),
        shape: shape.iter().map(|value| (*value).into()).collect(),
    };
    let valid = onnx.inputs
        == [
            tensor("pixel_values", "float32", &["1", "3", "800", "800"]),
            tensor("input_ids", "int64", &["1", "sequence_length"]),
            tensor("token_type_ids", "int64", &["1", "sequence_length"]),
            tensor("attention_mask", "int64", &["1", "sequence_length"]),
            tensor("pixel_mask", "int64", &["1", "800", "800"]),
        ]
        && onnx.outputs
            == [
                tensor("logits", "float32", &["1", "900", "256"]),
                tensor("pred_boxes", "float32", &["1", "900", "4"]),
            ]
        && image.identity == "grounding_dino_exact_800_rgb_imagenet_v1"
        && image.orientation == VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS
        && image.resize == "exact_800x800_bilinear"
        && image.channel_order == "rgb"
        && image.layout == "nchw"
        && image.dtype == "float32"
        && image.mean == [0.485, 0.456, 0.406]
        && image.scale == [0.229, 0.224, 0.225]
        && text.identity == "grounding_dino_bert_uncased_period_truncate256_v1"
        && text.max_length == LOGIT_TOKEN_COUNT
        && text.lowercase
        && text.padding == "longest"
        && text.truncation
        && onnx.postprocessing_identity == "sigmoid_phrase_score_box_clip_nms_normalized_v1";
    if valid {
        Ok(())
    } else {
        Err(ProviderError::Protocol(
            "Grounding DINO Build contract does not match the typed adapter".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_suppression_uses_normalized_boxes() {
        let left = NormalizedBoundingBox {
            x: 0.1,
            y: 0.1,
            width: 0.4,
            height: 0.4,
        };
        let overlap = NormalizedBoundingBox {
            x: 0.12,
            y: 0.12,
            width: 0.4,
            height: 0.4,
        };
        let separate = NormalizedBoundingBox {
            x: 0.7,
            y: 0.7,
            width: 0.2,
            height: 0.2,
        };
        assert!(intersection_over_union(&left, &overlap) > 0.7);
        assert_eq!(intersection_over_union(&left, &separate), 0.0);
    }
}

use image::{RgbImage, imageops::FilterType};
use infer_core::{
    BoundingBox, FaceDetection, FaceDetectionRequest, FivePointLandmarks, ImageGeometry, Point,
    VISION_ORIENTATION_INPUT_PIXELS_NO_EXIF_TRANSFORM,
};
use ort::{session::RunOptions, value::Tensor};

use super::{
    FaceDetectionExecutionOutput, SessionEntry, decode_image, execution_provenance, parameter,
    tensor_data,
};
use crate::ProviderError;

pub(super) fn run(
    entry: &SessionEntry,
    request: FaceDetectionRequest,
    run_options: &RunOptions,
) -> Result<FaceDetectionExecutionOutput, ProviderError> {
    let decoded = decode_image(&request.image)?;
    let (original_width, original_height) = decoded.dimensions();
    let (resized, scale) = resize_for_yunet(decoded);
    let (width, height) = resized.dimensions();
    let padded_width = width.div_ceil(32) * 32;
    let padded_height = height.div_ceil(32) * 32;
    let input = yunet_tensor(&resized, padded_width, padded_height)?;

    let mut session = entry.session.lock().expect("ONNX session poisoned");
    let outputs = session
        .run_with_options(ort::inputs!["input" => input], run_options)
        .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?;
    let onnx = entry.build.onnx.as_ref().expect("validated ONNX build");
    let score_threshold = parameter(onnx, "score_threshold", 0.6)? as f32;
    let nms_threshold = parameter(onnx, "nms_threshold", 0.3)? as f32;
    let top_k = parameter(onnx, "top_k", 5_000.0)? as usize;
    let mut candidates = decode_yunet(&outputs, padded_width, padded_height, score_threshold)?;
    candidates.sort_by(|left, right| right.confidence.total_cmp(&left.confidence));
    candidates.truncate(top_k);
    let detections = nms(candidates, nms_threshold)
        .into_iter()
        .filter_map(|candidate| candidate.into_public(original_width, original_height, scale))
        .collect();

    Ok(FaceDetectionExecutionOutput {
        image: ImageGeometry {
            width: original_width,
            height: original_height,
            orientation: VISION_ORIENTATION_INPUT_PIXELS_NO_EXIF_TRANSFORM.into(),
        },
        detections,
        provenance: execution_provenance(entry),
    })
}

fn resize_for_yunet(image: RgbImage) -> (RgbImage, f32) {
    let (width, height) = image.dimensions();
    let longest = width.max(height);
    if longest <= 1_600 {
        return (image, 1.0);
    }
    let scale = 1_600.0 / longest as f32;
    let resized_width = ((width as f32 * scale).round() as u32).max(1);
    let resized_height = ((height as f32 * scale).round() as u32).max(1);
    (
        image::imageops::resize(&image, resized_width, resized_height, FilterType::Triangle),
        scale,
    )
}

fn yunet_tensor(
    image: &RgbImage,
    padded_width: u32,
    padded_height: u32,
) -> Result<Tensor<f32>, ProviderError> {
    let plane = (padded_width as usize) * (padded_height as usize);
    let mut data = vec![0.0_f32; plane * 3];
    for (x, y, pixel) in image.enumerate_pixels() {
        let offset = y as usize * padded_width as usize + x as usize;
        data[offset] = pixel[2] as f32;
        data[plane + offset] = pixel[1] as f32;
        data[2 * plane + offset] = pixel[0] as f32;
    }
    Tensor::from_array((
        [1_usize, 3, padded_height as usize, padded_width as usize],
        data,
    ))
    .map_err(|error| ProviderError::NativeRuntime(error.to_string()))
}

#[derive(Clone)]
struct CandidateFace {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    landmarks: [Point; 5],
    confidence: f32,
}

fn decode_yunet(
    outputs: &ort::session::SessionOutputs<'_>,
    width: u32,
    height: u32,
    score_threshold: f32,
) -> Result<Vec<CandidateFace>, ProviderError> {
    let mut candidates = Vec::new();
    for stride in [8_usize, 16, 32] {
        let cls = tensor_data(outputs, &format!("cls_{stride}"))?;
        let obj = tensor_data(outputs, &format!("obj_{stride}"))?;
        let bbox = tensor_data(outputs, &format!("bbox_{stride}"))?;
        let kps = tensor_data(outputs, &format!("kps_{stride}"))?;
        let rows = height as usize / stride;
        let cols = width as usize / stride;
        let cells = rows * cols;
        if cls.len() != cells
            || obj.len() != cells
            || bbox.len() != cells * 4
            || kps.len() != cells * 10
        {
            return Err(ProviderError::Protocol(format!(
                "YuNet output shape mismatch for stride {stride}"
            )));
        }
        for row in 0..rows {
            for column in 0..cols {
                let index = row * cols + column;
                let score = cls[index].clamp(0.0, 1.0) * obj[index].clamp(0.0, 1.0);
                let confidence = score.sqrt();
                if confidence < score_threshold {
                    continue;
                }
                let base = index * 4;
                let stride = stride as f32;
                let center_x = (column as f32 + bbox[base]) * stride;
                let center_y = (row as f32 + bbox[base + 1]) * stride;
                let box_width = bbox[base + 2].exp() * stride;
                let box_height = bbox[base + 3].exp() * stride;
                let mut landmarks = [Point { x: 0.0, y: 0.0 }; 5];
                for (landmark, point) in landmarks.iter_mut().enumerate() {
                    let point_base = index * 10 + landmark * 2;
                    point.x = (column as f32 + kps[point_base]) * stride;
                    point.y = (row as f32 + kps[point_base + 1]) * stride;
                }
                candidates.push(CandidateFace {
                    x: center_x - box_width / 2.0,
                    y: center_y - box_height / 2.0,
                    width: box_width,
                    height: box_height,
                    landmarks,
                    confidence,
                });
            }
        }
    }
    Ok(candidates)
}

fn nms(mut candidates: Vec<CandidateFace>, threshold: f32) -> Vec<CandidateFace> {
    let mut kept = Vec::new();
    while let Some(candidate) = candidates.first().cloned() {
        candidates.remove(0);
        candidates.retain(|other| intersection_over_union(&candidate, other) <= threshold);
        kept.push(candidate);
    }
    kept
}

fn intersection_over_union(left: &CandidateFace, right: &CandidateFace) -> f32 {
    let x1 = left.x.max(right.x);
    let y1 = left.y.max(right.y);
    let x2 = (left.x + left.width).min(right.x + right.width);
    let y2 = (left.y + left.height).min(right.y + right.height);
    let intersection = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    let union = left.width * left.height + right.width * right.height - intersection;
    if union <= 0.0 {
        0.0
    } else {
        intersection / union
    }
}

impl CandidateFace {
    fn into_public(self, width: u32, height: u32, scale: f32) -> Option<FaceDetection> {
        let inverse = 1.0 / scale;
        let max_x = width as f32;
        let max_y = height as f32;
        let x1 = (self.x * inverse).clamp(0.0, max_x);
        let y1 = (self.y * inverse).clamp(0.0, max_y);
        let x2 = ((self.x + self.width) * inverse).clamp(0.0, max_x);
        let y2 = ((self.y + self.height) * inverse).clamp(0.0, max_y);
        if x2 <= x1 || y2 <= y1 {
            return None;
        }
        let points = self.landmarks.map(|point| Point {
            x: (point.x * inverse).clamp(0.0, max_x),
            y: (point.y * inverse).clamp(0.0, max_y),
        });
        Some(FaceDetection {
            bounding_box: BoundingBox {
                x: x1,
                y: y1,
                width: x2 - x1,
                height: y2 - y1,
            },
            landmarks: FivePointLandmarks {
                right_eye: points[0],
                left_eye: points[1],
                nose_tip: points[2],
                right_mouth_corner: points[3],
                left_mouth_corner: points[4],
            },
            confidence: self.confidence,
        })
    }
}

#[cfg(test)]
mod tests {
    use infer_core::Point;

    use super::*;

    #[test]
    fn nms_keeps_the_highest_confidence_overlap() {
        let face = |x, confidence| CandidateFace {
            x,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            landmarks: [Point { x: 1.0, y: 1.0 }; 5],
            confidence,
        };
        let result = nms(vec![face(0.0, 0.9), face(1.0, 0.8), face(30.0, 0.7)], 0.3);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].confidence, 0.9);
    }

    #[test]
    fn public_coordinates_are_clipped_and_scaled_back() {
        let face = CandidateFace {
            x: -2.0,
            y: 2.0,
            width: 12.0,
            height: 12.0,
            landmarks: [Point { x: 5.0, y: 5.0 }; 5],
            confidence: 0.9,
        };
        let public = face.into_public(20, 20, 0.5).unwrap();
        assert_eq!(public.bounding_box.x, 0.0);
        assert_eq!(public.bounding_box.width, 20.0);
    }
}

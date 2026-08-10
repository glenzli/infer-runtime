use image::{Rgb, RgbImage};
use infer_core::{FaceEmbeddingEligibility, FaceEmbeddingRequest, FaceEmbeddingVector, Point};
use ort::{session::RunOptions, value::Tensor};

use super::{
    FaceEmbeddingExecutionOutput, SessionEntry, decode_image, execution_provenance, tensor_data,
};
use crate::ProviderError;

const ALIGNED_SIZE: u32 = 112;
const EMBEDDING_DIMENSIONS: usize = 128;
const MIN_INTER_EYE_DISTANCE_PIXELS: f32 = 4.0;
const MAX_ALIGNMENT_RMSE_PIXELS: f32 = 5.0;

// OpenCV SFace's pinned five-point template, in the same YuNet point order as
// the public contract: right eye, left eye, nose, right mouth, left mouth.
const TARGET: [Point; 5] = [
    Point {
        x: 38.2946,
        y: 51.6963,
    },
    Point {
        x: 73.5318,
        y: 51.5014,
    },
    Point {
        x: 56.0252,
        y: 71.7366,
    },
    Point {
        x: 41.5493,
        y: 92.3655,
    },
    Point {
        x: 70.7299,
        y: 92.2041,
    },
];

pub(super) fn run(
    entry: &SessionEntry,
    request: FaceEmbeddingRequest,
    run_options: &RunOptions,
) -> Result<FaceEmbeddingExecutionOutput, ProviderError> {
    let image = decode_image(&request.image)?;
    let (width, height) = image.dimensions();
    let landmarks = request.landmarks.points();
    let landmarks_in_image = landmarks.iter().all(|point| {
        point.x >= 0.0 && point.y >= 0.0 && point.x < width as f32 && point.y < height as f32
    });
    if !landmarks_in_image {
        return Err(ProviderError::InvalidInput(
            "face embedding landmarks must be inside the decoded image".into(),
        ));
    }
    let inter_eye_distance = distance(landmarks[0], landmarks[1]);
    if inter_eye_distance < MIN_INTER_EYE_DISTANCE_PIXELS {
        return Err(ProviderError::InvalidInput(format!(
            "face embedding inter-eye distance must be at least {MIN_INTER_EYE_DISTANCE_PIXELS} pixels"
        )));
    }
    let transform = SimilarityTransform::fit(landmarks, TARGET)?;
    let alignment_rmse = transform.rmse(landmarks, TARGET);
    if !alignment_rmse.is_finite() || alignment_rmse > MAX_ALIGNMENT_RMSE_PIXELS {
        return Err(ProviderError::InvalidInput(format!(
            "face embedding landmarks do not fit the SFace template within {MAX_ALIGNMENT_RMSE_PIXELS} pixels"
        )));
    }
    let aligned = warp_similarity(&image, transform)?;
    let input = sface_tensor(&aligned)?;

    let mut session = entry.session.lock().expect("ONNX session poisoned");
    let outputs = session
        .run_with_options(ort::inputs!["data" => input], run_options)
        .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?;
    let raw = tensor_data(&outputs, "fc1")?;
    if raw.len() != EMBEDDING_DIMENSIONS || raw.iter().any(|value| !value.is_finite()) {
        return Err(ProviderError::Protocol(
            "SFace output must contain exactly 128 finite float32 values".into(),
        ));
    }
    let norm = raw.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() || norm <= f32::EPSILON {
        return Err(ProviderError::Protocol(
            "SFace output cannot be L2-normalized".into(),
        ));
    }
    let values = raw.iter().map(|value| value / norm).collect();
    let onnx = entry.build.onnx.as_ref().expect("validated ONNX build");
    let explicit_space = onnx.embedding_space.as_ref();
    let space_identity = explicit_space
        .map(|space| space.identity.clone())
        .unwrap_or_else(|| {
            format!(
                "{}:{}:{}",
                entry.build_id, onnx.artifact.sha256, onnx.postprocessing_identity
            )
        });

    Ok(FaceEmbeddingExecutionOutput {
        embedding: FaceEmbeddingVector {
            values,
            dimensions: explicit_space.map_or(EMBEDDING_DIMENSIONS, |space| space.dimensions),
            normalized: explicit_space.is_none_or(|space| space.normalized),
            distance_metric: explicit_space
                .map_or_else(|| "cosine".into(), |space| space.distance_metric.clone()),
            space: space_identity,
        },
        eligibility: FaceEmbeddingEligibility {
            eligible: true,
            landmarks_in_image,
            inter_eye_distance_pixels: inter_eye_distance,
            alignment_rmse_pixels: alignment_rmse,
        },
        provenance: execution_provenance(entry),
    })
}

#[derive(Debug, Clone, Copy)]
struct SimilarityTransform {
    a: f32,
    b: f32,
    tx: f32,
    ty: f32,
}

impl SimilarityTransform {
    fn fit(source: [Point; 5], destination: [Point; 5]) -> Result<Self, ProviderError> {
        let source_mean = mean(source);
        let destination_mean = mean(destination);
        let mut denominator = 0.0;
        let mut a_numerator = 0.0;
        let mut b_numerator = 0.0;
        for (source, destination) in source.into_iter().zip(destination) {
            let sx = source.x - source_mean.x;
            let sy = source.y - source_mean.y;
            let dx = destination.x - destination_mean.x;
            let dy = destination.y - destination_mean.y;
            denominator += sx * sx + sy * sy;
            a_numerator += sx * dx + sy * dy;
            b_numerator += sx * dy - sy * dx;
        }
        if !denominator.is_finite() || denominator <= f32::EPSILON {
            return Err(ProviderError::InvalidInput(
                "face embedding landmarks are degenerate".into(),
            ));
        }
        let a = a_numerator / denominator;
        let b = b_numerator / denominator;
        let transform = Self {
            a,
            b,
            tx: destination_mean.x - a * source_mean.x + b * source_mean.y,
            ty: destination_mean.y - b * source_mean.x - a * source_mean.y,
        };
        let determinant = a * a + b * b;
        if !determinant.is_finite() || determinant <= f32::EPSILON {
            return Err(ProviderError::InvalidInput(
                "face embedding alignment transform is singular".into(),
            ));
        }
        Ok(transform)
    }

    fn apply(self, point: Point) -> Point {
        Point {
            x: self.a * point.x - self.b * point.y + self.tx,
            y: self.b * point.x + self.a * point.y + self.ty,
        }
    }

    fn inverse(self, point: Point) -> Point {
        let determinant = self.a * self.a + self.b * self.b;
        let x = point.x - self.tx;
        let y = point.y - self.ty;
        Point {
            x: (self.a * x + self.b * y) / determinant,
            y: (-self.b * x + self.a * y) / determinant,
        }
    }

    fn rmse(self, source: [Point; 5], destination: [Point; 5]) -> f32 {
        (source
            .into_iter()
            .zip(destination)
            .map(|(source, destination)| {
                let actual = self.apply(source);
                (actual.x - destination.x).powi(2) + (actual.y - destination.y).powi(2)
            })
            .sum::<f32>()
            / 5.0)
            .sqrt()
    }
}

fn mean(points: [Point; 5]) -> Point {
    let (x, y) = points
        .into_iter()
        .fold((0.0, 0.0), |(x, y), point| (x + point.x, y + point.y));
    Point {
        x: x / 5.0,
        y: y / 5.0,
    }
}

fn distance(left: Point, right: Point) -> f32 {
    ((left.x - right.x).powi(2) + (left.y - right.y).powi(2)).sqrt()
}

fn warp_similarity(
    image: &RgbImage,
    transform: SimilarityTransform,
) -> Result<RgbImage, ProviderError> {
    let mut aligned = RgbImage::new(ALIGNED_SIZE, ALIGNED_SIZE);
    for y in 0..ALIGNED_SIZE {
        for x in 0..ALIGNED_SIZE {
            let source = transform.inverse(Point {
                x: x as f32,
                y: y as f32,
            });
            aligned.put_pixel(x, y, bilinear(image, source.x, source.y));
        }
    }
    Ok(aligned)
}

fn bilinear(image: &RgbImage, x: f32, y: f32) -> Rgb<u8> {
    if x < 0.0 || y < 0.0 || x > (image.width() - 1) as f32 || y > (image.height() - 1) as f32 {
        return Rgb([0, 0, 0]);
    }
    let x0 = x.floor() as u32;
    let y0 = y.floor() as u32;
    let x1 = (x0 + 1).min(image.width() - 1);
    let y1 = (y0 + 1).min(image.height() - 1);
    let dx = x - x0 as f32;
    let dy = y - y0 as f32;
    let mut channels = [0_u8; 3];
    for (channel, output) in channels.iter_mut().enumerate() {
        let top = image.get_pixel(x0, y0)[channel] as f32 * (1.0 - dx)
            + image.get_pixel(x1, y0)[channel] as f32 * dx;
        let bottom = image.get_pixel(x0, y1)[channel] as f32 * (1.0 - dx)
            + image.get_pixel(x1, y1)[channel] as f32 * dx;
        *output = (top * (1.0 - dy) + bottom * dy).round().clamp(0.0, 255.0) as u8;
    }
    Rgb(channels)
}

fn sface_tensor(image: &RgbImage) -> Result<Tensor<f32>, ProviderError> {
    let plane = (ALIGNED_SIZE * ALIGNED_SIZE) as usize;
    let mut data = vec![0.0_f32; plane * 3];
    for (x, y, pixel) in image.enumerate_pixels() {
        let offset = y as usize * ALIGNED_SIZE as usize + x as usize;
        data[offset] = pixel[0] as f32;
        data[plane + offset] = pixel[1] as f32;
        data[2 * plane + offset] = pixel[2] as f32;
    }
    Tensor::from_array(([1_usize, 3, 112, 112], data))
        .map_err(|error| ProviderError::NativeRuntime(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_landmarks_produce_identity_transform() {
        let transform = SimilarityTransform::fit(TARGET, TARGET).unwrap();
        assert!((transform.a - 1.0).abs() < 1e-5);
        assert!(transform.b.abs() < 1e-5);
        assert!(transform.tx.abs() < 1e-4);
        assert!(transform.ty.abs() < 1e-4);
        assert!(transform.rmse(TARGET, TARGET) < 1e-4);
    }

    #[test]
    fn scaled_translated_landmarks_map_to_canonical_template() {
        let source = TARGET.map(|point| Point {
            x: point.x * 2.0 + 10.0,
            y: point.y * 2.0 + 20.0,
        });
        let transform = SimilarityTransform::fit(source, TARGET).unwrap();
        assert!(transform.rmse(source, TARGET) < 1e-4);
    }

    #[test]
    fn rotated_landmarks_map_to_canonical_template() {
        let angle = 15.0_f32.to_radians();
        let (sin, cos) = angle.sin_cos();
        let source = TARGET.map(|point| Point {
            x: 1.5 * (cos * point.x - sin * point.y) + 12.0,
            y: 1.5 * (sin * point.x + cos * point.y) + 7.0,
        });
        let transform = SimilarityTransform::fit(source, TARGET).unwrap();
        assert!(transform.rmse(source, TARGET) < 1e-3);
    }

    #[test]
    fn degenerate_landmarks_are_rejected() {
        let source = [Point { x: 2.0, y: 2.0 }; 5];
        assert!(SimilarityTransform::fit(source, TARGET).is_err());
    }
}

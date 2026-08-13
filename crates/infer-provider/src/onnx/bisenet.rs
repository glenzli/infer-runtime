use std::io::Cursor;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{DynamicImage, GrayImage, ImageFormat, Luma, RgbImage, imageops::FilterType};
use infer_core::{
    BoundingBox, EncodedLabelMap, FACE_PARSING_ONTOLOGY_ID, FACE_PARSING_ONTOLOGY_REVISION,
    FaceParsingOntology, FaceParsingRegion, FaceParsingRequest, ImageGeometry,
    MAX_FACE_PARSING_PIXELS, MAX_SEGMENTATION_MASK_BYTES,
    VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS,
};
use ort::{session::RunOptions, value::Tensor};
use sha2::{Digest, Sha256};

use super::{
    FaceParsingExecutionOutput, SessionEntry, decode_image, execution_provenance, tensor_data,
};
use crate::ProviderError;

const INPUT_SIZE: usize = 512;
const CLASS_COUNT: usize = 19;
const FACE_CONTEXT_SCALE: f32 = 1.8;

const CLASS_IDS: [&str; CLASS_COUNT] = [
    "background",
    "skin",
    "left_eyebrow",
    "right_eyebrow",
    "left_eye",
    "right_eye",
    "eyeglasses",
    "left_ear",
    "right_ear",
    "earring",
    "nose",
    "mouth",
    "upper_lip",
    "lower_lip",
    "neck",
    "necklace",
    "clothing",
    "hair",
    "hat",
];

pub(super) fn run(
    entry: &SessionEntry,
    request: FaceParsingRequest,
    run_options: &RunOptions,
) -> Result<FaceParsingExecutionOutput, ProviderError> {
    validate_build_contract(entry)?;
    let decoded = decode_image(&request.image)?;
    let (width, height) = decoded.dimensions();
    if u64::from(width) * u64::from(height) > MAX_FACE_PARSING_PIXELS {
        return Err(ProviderError::InvalidInput(format!(
            "face parsing image exceeds {MAX_FACE_PARSING_PIXELS} pixels"
        )));
    }
    validate_face_box(request.face_box.clone(), width, height)?;
    let crop = expanded_face_crop(request.face_box.clone(), width, height);
    let input = input_tensor(&decoded, crop)?;

    let mut session = entry.session.lock().expect("ONNX session poisoned");
    let outputs = session
        .run_with_options(ort::inputs!["input" => input], run_options)
        .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?;
    let logits = tensor_data(&outputs, "output")?;
    if logits.len() != CLASS_COUNT * INPUT_SIZE * INPUT_SIZE {
        return Err(ProviderError::Protocol(
            "BiSeNet output shape does not match the 19-class Build contract".into(),
        ));
    }

    let label_map = restore_label_map(logits, width, height, crop);
    let regions = summarize_regions(&label_map);
    let encoded = encode_label_map(&label_map)?;
    Ok(FaceParsingExecutionOutput {
        image: ImageGeometry {
            width,
            height,
            orientation: VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS.into(),
        },
        face_box: request.face_box,
        label_map: encoded,
        ontology: FaceParsingOntology {
            id: FACE_PARSING_ONTOLOGY_ID.into(),
            revision: FACE_PARSING_ONTOLOGY_REVISION.into(),
            background_value: 0,
            class_count: CLASS_COUNT,
        },
        regions,
        provenance: execution_provenance(entry),
    })
}

fn validate_build_contract(entry: &SessionEntry) -> Result<(), ProviderError> {
    let onnx = entry
        .build
        .onnx
        .as_ref()
        .ok_or_else(|| ProviderError::Protocol("BiSeNet Build has no ONNX contract".into()))?;
    let preprocess = onnx.preprocessing.as_ref().ok_or_else(|| {
        ProviderError::Protocol("BiSeNet Build has no preprocessing contract".into())
    })?;
    let tensor = |name: &str, dtype: &str, shape: &[&str]| infer_core::TensorContractConfig {
        name: name.into(),
        dtype: dtype.into(),
        shape: shape.iter().map(|value| (*value).into()).collect(),
    };
    let valid = onnx.inputs == [tensor("input", "float32", &["batch", "3", "512", "512"])]
        && onnx.outputs
            == [
                tensor(
                    "output",
                    "float32",
                    &["batch", "classes", "height", "width"],
                ),
                tensor("414", "float32", &["batch", "classes", "height", "width"]),
                tensor("424", "float32", &["batch", "classes", "height", "width"]),
            ]
        && preprocess.identity == "bisenet_resnet18_celeba_512_rgb_imagenet_v1"
        && preprocess.orientation == VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS
        && preprocess.resize == "face_box_context_1_8_then_triangle_512x512"
        && preprocess.color_space == "srgb"
        && preprocess.channel_order == "rgb"
        && preprocess.layout == "nchw"
        && preprocess.dtype == "float32"
        && preprocess.mean == [0.485, 0.456, 0.406]
        && preprocess.scale == [0.229, 0.224, 0.225]
        && onnx.postprocessing_identity
            == "argmax_19_then_nearest_restore_full_image_indexed_png_v1";
    if valid {
        Ok(())
    } else {
        Err(ProviderError::Protocol(
            "BiSeNet Build contract does not match the typed adapter".into(),
        ))
    }
}

fn validate_face_box(face_box: BoundingBox, width: u32, height: u32) -> Result<(), ProviderError> {
    if !face_box.is_finite_positive()
        || face_box.x + face_box.width > width as f32
        || face_box.y + face_box.height > height as f32
    {
        return Err(ProviderError::InvalidInput(
            "face_box must fit within the submitted image".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct CropBounds {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

fn expanded_face_crop(face_box: BoundingBox, width: u32, height: u32) -> CropBounds {
    let center_x = face_box.x + face_box.width / 2.0;
    let center_y = face_box.y + face_box.height / 2.0;
    let side = face_box.width.max(face_box.height) * FACE_CONTEXT_SCALE;
    let x0 = (center_x - side / 2.0).floor().max(0.0) as u32;
    let y0 = (center_y - side / 2.0).floor().max(0.0) as u32;
    let x1 = (center_x + side / 2.0).ceil().min(width as f32) as u32;
    let y1 = (center_y + side / 2.0).ceil().min(height as f32) as u32;
    CropBounds {
        x: x0,
        y: y0,
        width: (x1 - x0).max(1),
        height: (y1 - y0).max(1),
    }
}

fn input_tensor(image: &RgbImage, crop: CropBounds) -> Result<Tensor<f32>, ProviderError> {
    let cropped =
        image::imageops::crop_imm(image, crop.x, crop.y, crop.width, crop.height).to_image();
    let resized = image::imageops::resize(
        &cropped,
        INPUT_SIZE as u32,
        INPUT_SIZE as u32,
        FilterType::Triangle,
    );
    let plane = INPUT_SIZE * INPUT_SIZE;
    let mut data = vec![0.0_f32; plane * 3];
    let mean = [0.485_f32, 0.456, 0.406];
    let std = [0.229_f32, 0.224, 0.225];
    for (x, y, pixel) in resized.enumerate_pixels() {
        let offset = y as usize * INPUT_SIZE + x as usize;
        for channel in 0..3 {
            data[channel * plane + offset] =
                (pixel[channel] as f32 / 255.0 - mean[channel]) / std[channel];
        }
    }
    Tensor::from_array(([1_usize, 3, INPUT_SIZE, INPUT_SIZE], data))
        .map_err(|error| ProviderError::NativeRuntime(error.to_string()))
}

fn restore_label_map(logits: &[f32], width: u32, height: u32, crop: CropBounds) -> GrayImage {
    let plane = INPUT_SIZE * INPUT_SIZE;
    let mut labels = GrayImage::new(width, height);
    for local_y in 0..crop.height {
        let source_y = ((u64::from(local_y) * INPUT_SIZE as u64) / u64::from(crop.height))
            .min((INPUT_SIZE - 1) as u64) as usize;
        for local_x in 0..crop.width {
            let source_x = ((u64::from(local_x) * INPUT_SIZE as u64) / u64::from(crop.width))
                .min((INPUT_SIZE - 1) as u64) as usize;
            let offset = source_y * INPUT_SIZE + source_x;
            let mut best_class = 0_u8;
            let mut best_score = logits[offset];
            for class in 1..CLASS_COUNT {
                let score = logits[class * plane + offset];
                if score > best_score {
                    best_score = score;
                    best_class = class as u8;
                }
            }
            labels.put_pixel(crop.x + local_x, crop.y + local_y, Luma([best_class]));
        }
    }
    labels
}

fn summarize_regions(labels: &GrayImage) -> Vec<FaceParsingRegion> {
    let mut counts = [0_u64; CLASS_COUNT];
    let mut bounds = [None::<(u32, u32, u32, u32)>; CLASS_COUNT];
    for (x, y, pixel) in labels.enumerate_pixels() {
        let class = usize::from(pixel[0]);
        if class >= CLASS_COUNT {
            continue;
        }
        counts[class] += 1;
        bounds[class] = Some(match bounds[class] {
            Some((min_x, min_y, max_x, max_y)) => {
                (min_x.min(x), min_y.min(y), max_x.max(x), max_y.max(y))
            }
            None => (x, y, x, y),
        });
    }
    (0..CLASS_COUNT)
        .map(|class| FaceParsingRegion {
            class_id: format!("{FACE_PARSING_ONTOLOGY_ID}:{}", CLASS_IDS[class]),
            label: CLASS_IDS[class].replace('_', " "),
            label_value: class as u8,
            pixel_count: counts[class],
            bounding_box: bounds[class].map(|(min_x, min_y, max_x, max_y)| BoundingBox {
                x: min_x as f32,
                y: min_y as f32,
                width: (max_x - min_x + 1) as f32,
                height: (max_y - min_y + 1) as f32,
            }),
        })
        .collect()
}

fn encode_label_map(labels: &GrayImage) -> Result<EncodedLabelMap, ProviderError> {
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageLuma8(labels.clone())
        .write_to(&mut bytes, ImageFormat::Png)
        .map_err(|_| ProviderError::Protocol("face parsing label-map encoding failed".into()))?;
    let bytes = bytes.into_inner();
    if bytes.len() > MAX_SEGMENTATION_MASK_BYTES {
        return Err(ProviderError::Protocol(
            "face parsing label-map exceeds the response limit".into(),
        ));
    }
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    Ok(EncodedLabelMap {
        content_type: "image/png".into(),
        encoding: "indexed_u8_png".into(),
        data_base64: STANDARD.encode(bytes),
        sha256,
        width: labels.width(),
        height: labels.height(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn face_context_crop_is_bounded_at_image_edges() {
        let crop = expanded_face_crop(
            BoundingBox {
                x: 0.0,
                y: 1.0,
                width: 20.0,
                height: 30.0,
            },
            100,
            80,
        );
        assert_eq!(crop.x, 0);
        assert_eq!(crop.y, 0);
        assert!(crop.x + crop.width <= 100);
        assert!(crop.y + crop.height <= 80);
    }

    #[test]
    fn label_map_summary_preserves_stable_class_ids() {
        let mut labels = GrayImage::new(2, 1);
        labels.put_pixel(0, 0, Luma([1]));
        labels.put_pixel(1, 0, Luma([17]));
        let regions = summarize_regions(&labels);
        assert_eq!(regions[1].class_id, "celebamask_hq_19:skin");
        assert_eq!(regions[17].class_id, "celebamask_hq_19:hair");
        assert_eq!(regions[17].pixel_count, 1);
    }
}

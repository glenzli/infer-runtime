use std::io::Cursor;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{DynamicImage, GrayImage, ImageFormat, Luma, Rgb, RgbImage, imageops::FilterType};
use infer_core::{
    EncodedCompletedRaster, IMAGE_COMPLETION_EDGE, ImageCompletionRequest, ImageGeometry,
    MAX_SEGMENTATION_MASK_BYTES, VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS,
};
use ort::{session::RunOptions, value::Tensor};
use sha2::{Digest, Sha256};

use super::{
    ImageCompletionExecutionOutput, SessionEntry, decode_image, execution_provenance, tensor_data,
};
use crate::ProviderError;

const EDGE: usize = IMAGE_COMPLETION_EDGE as usize;

pub(super) fn run(
    entry: &SessionEntry,
    request: ImageCompletionRequest,
    run_options: &RunOptions,
) -> Result<ImageCompletionExecutionOutput, ProviderError> {
    validate_build_contract(entry)?;
    let decoded = decode_image(&request.image)?;
    let (width, height) = decoded.dimensions();
    let mask = decode_mask(&request.mask.bytes)?;
    if mask.dimensions() != (width, height) {
        return Err(ProviderError::InvalidInput(
            "image completion mask dimensions must match the image".into(),
        ));
    }
    if !mask.pixels().any(|sample| sample[0] != 0) {
        return Err(ProviderError::InvalidInput(
            "image completion mask must select at least one pixel".into(),
        ));
    }
    let resized_image = image::imageops::resize(
        &decoded,
        IMAGE_COMPLETION_EDGE,
        IMAGE_COMPLETION_EDGE,
        FilterType::Triangle,
    );
    let resized_mask = image::imageops::resize(
        &mask,
        IMAGE_COMPLETION_EDGE,
        IMAGE_COMPLETION_EDGE,
        FilterType::Nearest,
    );
    let image_input = image_tensor(&resized_image)?;
    let mask_input = mask_tensor(&resized_mask)?;
    let mut session = entry.session.lock().expect("ONNX session poisoned");
    let outputs = session
        .run_with_options(
            ort::inputs!["image" => image_input, "mask" => mask_input],
            run_options,
        )
        .map_err(|error| ProviderError::NativeRuntime(error.to_string()))?;
    let output = tensor_data(&outputs, "output")?;
    if output.len() != 3 * EDGE * EDGE || output.iter().any(|value| !value.is_finite()) {
        return Err(ProviderError::Protocol(
            "LaMa output shape or samples do not match the typed adapter".into(),
        ));
    }
    let raster = encode_composited_output(output, &resized_image, &resized_mask)?;
    Ok(ImageCompletionExecutionOutput {
        input_coordinate_extent: ImageGeometry {
            width,
            height,
            orientation: VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS.into(),
        },
        raster,
        provenance: execution_provenance(entry),
    })
}

fn decode_mask(bytes: &[u8]) -> Result<GrayImage, ProviderError> {
    let decoded = image::load_from_memory_with_format(bytes, ImageFormat::Png)
        .map_err(|error| ProviderError::InvalidInput(error.to_string()))?;
    let mask = match decoded {
        DynamicImage::ImageLuma8(mask) => mask,
        DynamicImage::ImageLumaA8(mask) => {
            GrayImage::from_fn(mask.width(), mask.height(), |x, y| {
                Luma([mask.get_pixel(x, y)[1]])
            })
        }
        DynamicImage::ImageRgba8(mask) => {
            GrayImage::from_fn(mask.width(), mask.height(), |x, y| {
                Luma([mask.get_pixel(x, y)[3]])
            })
        }
        other => other.to_luma8(),
    };
    Ok(mask)
}

fn image_tensor(image: &RgbImage) -> Result<Tensor<f32>, ProviderError> {
    let plane = EDGE * EDGE;
    let mut data = vec![0.0_f32; plane * 3];
    for (x, y, pixel) in image.enumerate_pixels() {
        let offset = y as usize * EDGE + x as usize;
        for channel in 0..3 {
            data[channel * plane + offset] = pixel[channel] as f32 / 255.0;
        }
    }
    Tensor::from_array(([1_usize, 3, EDGE, EDGE], data))
        .map_err(|error| ProviderError::NativeRuntime(error.to_string()))
}

fn mask_tensor(mask: &GrayImage) -> Result<Tensor<f32>, ProviderError> {
    let data = mask
        .pixels()
        .map(|sample| if sample[0] == 0 { 0.0_f32 } else { 1.0_f32 })
        .collect::<Vec<_>>();
    Tensor::from_array(([1_usize, 1, EDGE, EDGE], data))
        .map_err(|error| ProviderError::NativeRuntime(error.to_string()))
}

fn encode_composited_output(
    output: &[f32],
    original: &RgbImage,
    mask: &GrayImage,
) -> Result<EncodedCompletedRaster, ProviderError> {
    let plane = EDGE * EDGE;
    let mut image = RgbImage::new(IMAGE_COMPLETION_EDGE, IMAGE_COMPLETION_EDGE);
    for y in 0..EDGE {
        for x in 0..EDGE {
            let offset = y * EDGE + x;
            let generated = Rgb([
                output[offset].round().clamp(0.0, 255.0) as u8,
                output[plane + offset].round().clamp(0.0, 255.0) as u8,
                output[2 * plane + offset].round().clamp(0.0, 255.0) as u8,
            ]);
            image.put_pixel(
                x as u32,
                y as u32,
                if mask.get_pixel(x as u32, y as u32)[0] == 0 {
                    *original.get_pixel(x as u32, y as u32)
                } else {
                    generated
                },
            );
        }
    }
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image)
        .write_to(&mut bytes, ImageFormat::Png)
        .map_err(|_| ProviderError::Protocol("image completion PNG encoding failed".into()))?;
    let bytes = bytes.into_inner();
    if bytes.len() > MAX_SEGMENTATION_MASK_BYTES {
        return Err(ProviderError::Protocol(
            "image completion raster exceeds the response limit".into(),
        ));
    }
    Ok(EncodedCompletedRaster {
        content_type: "image/png".into(),
        encoding: "srgb_rgb8_png_mask_bounded_v1".into(),
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        data_base64: STANDARD.encode(bytes),
        width: IMAGE_COMPLETION_EDGE,
        height: IMAGE_COMPLETION_EDGE,
    })
}

fn validate_build_contract(entry: &SessionEntry) -> Result<(), ProviderError> {
    let onnx = entry
        .build
        .onnx
        .as_ref()
        .ok_or_else(|| ProviderError::Protocol("LaMa Build has no ONNX contract".into()))?;
    let preprocess = onnx.preprocessing.as_ref().ok_or_else(|| {
        ProviderError::Protocol("LaMa Build has no image preprocessing contract".into())
    })?;
    let tensor = |name: &str, dtype: &str, shape: &[&str]| infer_core::TensorContractConfig {
        name: name.into(),
        dtype: dtype.into(),
        shape: shape.iter().map(|value| (*value).into()).collect(),
    };
    let valid = onnx.inputs
        == [
            tensor("image", "float32", &["batch", "3", "512", "512"]),
            tensor("mask", "float32", &["batch", "1", "512", "512"]),
        ]
        && onnx.outputs == [tensor("output", "float32", &["batch", "3", "512", "512"])]
        && preprocess.identity == "lama_exact_512_rgb_unit_mask_binary_v1"
        && preprocess.orientation == VISION_ORIENTATION_NORMALIZED_DISPLAY_PIXELS
        && preprocess.resize == "exact_512x512_triangle_image_nearest_mask"
        && preprocess.channel_order == "rgb"
        && preprocess.layout == "nchw"
        && preprocess.dtype == "float32"
        && preprocess.mean == [0.0, 0.0, 0.0]
        && preprocess.scale == [1.0, 1.0, 1.0]
        && onnx.postprocessing_identity == "clamp_rgb8_compose_selected_pixels_png_v1";
    if valid {
        Ok(())
    } else {
        Err(ProviderError::Protocol(
            "LaMa Build contract does not match the typed adapter".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_mask_uses_alpha_as_selection() {
        let mut rgba = image::RgbaImage::new(1, 1);
        rgba.put_pixel(0, 0, image::Rgba([255, 255, 255, 0]));
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(rgba)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        assert_eq!(
            decode_mask(&bytes.into_inner()).unwrap().get_pixel(0, 0)[0],
            0
        );
    }

    #[test]
    fn compositing_preserves_every_unselected_pixel() {
        let original = RgbImage::from_fn(IMAGE_COMPLETION_EDGE, IMAGE_COMPLETION_EDGE, |x, y| {
            Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        });
        let mask = GrayImage::from_fn(IMAGE_COMPLETION_EDGE, IMAGE_COMPLETION_EDGE, |x, y| {
            Luma([u8::from((192..320).contains(&x) && (192..320).contains(&y)) * 255])
        });
        let generated = vec![255.0_f32; 3 * EDGE * EDGE];
        let encoded = encode_composited_output(&generated, &original, &mask).unwrap();
        let bytes = STANDARD.decode(encoded.data_base64).unwrap();
        let completed = image::load_from_memory_with_format(&bytes, ImageFormat::Png)
            .unwrap()
            .to_rgb8();
        for (x, y, pixel) in original.enumerate_pixels() {
            if mask.get_pixel(x, y)[0] == 0 {
                assert_eq!(completed.get_pixel(x, y), pixel);
            }
        }
        assert_eq!(completed.get_pixel(256, 256), &Rgb([255, 255, 255]));
    }
}

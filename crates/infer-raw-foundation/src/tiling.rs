use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{PackedBayer, RawFoundationError, RawNindModel};

const TILE_EDGE: usize = 512;
const OUTPUT_SCALE: usize = 2;
const EXACT_HALO: usize = 100;
const BLEND_OVERLAP: usize = 112;
const STEP: usize = 288;
const STRIPE_FORMAT: &str = "shadow-linear-camera-rgb-f32-stripe-chw-v1";

#[derive(Debug, Clone, PartialEq)]
pub struct RawNindMaterializationReceipt {
    pub packed_width: usize,
    pub packed_height: usize,
    pub output_width: usize,
    pub output_height: usize,
    pub inference_passes: usize,
    pub tile_inferences: usize,
    pub global_input_mean: f64,
    pub first_pass_raw_output_mean: f64,
    pub second_pass_raw_output_mean: f64,
    pub global_gain: f64,
    pub sequence_sha256: String,
    pub payload_sha256: String,
    pub output: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RawNindStreamingReceipt {
    pub packed_width: usize,
    pub packed_height: usize,
    pub output_width: usize,
    pub output_height: usize,
    pub tile_inferences: usize,
    pub global_input_mean: f64,
    pub first_pass_raw_output_mean: f64,
    pub second_pass_raw_output_mean: f64,
    pub global_gain: f64,
    pub output_mean: f64,
    pub maximum_accumulator_rows: usize,
}

pub trait RawFoundationStripeSink {
    fn write_stripe(
        &mut self,
        y_start: usize,
        rows: usize,
        width: usize,
        channel_stride_rows: usize,
        values: &[f32],
    ) -> Result<(), RawFoundationError>;
}

#[derive(Clone, Copy)]
struct Placement {
    global_y0: usize,
    global_y1: usize,
    global_x0: usize,
    global_x1: usize,
    local_y0: usize,
    local_x0: usize,
}

pub fn materialize_two_pass(
    packed: &PackedBayer,
    model: &mut RawNindModel,
    cancellation: &CancellationToken,
) -> Result<RawNindMaterializationReceipt, RawFoundationError> {
    if packed.width < 2 || packed.height < 2 {
        return Err(RawFoundationError::InvalidRequest(
            "packed Bayer input is too small".into(),
        ));
    }
    let global_input_mean = packed
        .channels
        .iter()
        .flatten()
        .map(|value| f64::from(*value))
        .sum::<f64>()
        / (4 * packed.width * packed.height) as f64;
    let (first, first_tiles) = run_pass(packed, model, cancellation)?;
    let first_mean = mean(&first);
    if first_mean.abs() <= 1e-6 * global_input_mean.abs() {
        return Err(RawFoundationError::ModelContract(
            "RawNIND output mean cannot be gain matched".into(),
        ));
    }
    let global_gain = global_input_mean / first_mean;
    drop(first);
    let (mut output, second_tiles) = run_pass(packed, model, cancellation)?;
    let second_mean = mean(&output);
    for value in &mut output {
        *value *= global_gain as f32;
    }
    let (sequence_sha256, payload_sha256) =
        sequence_digests(&output, packed.height * 2, packed.width * 2);
    Ok(RawNindMaterializationReceipt {
        packed_width: packed.width,
        packed_height: packed.height,
        output_width: packed.width * 2,
        output_height: packed.height * 2,
        inference_passes: 2,
        tile_inferences: first_tiles + second_tiles,
        global_input_mean,
        first_pass_raw_output_mean: first_mean,
        second_pass_raw_output_mean: second_mean,
        global_gain,
        sequence_sha256,
        payload_sha256,
        output,
    })
}

pub fn materialize_two_pass_streaming(
    packed: &PackedBayer,
    model: &mut RawNindModel,
    cancellation: &CancellationToken,
    sink: &mut impl RawFoundationStripeSink,
) -> Result<RawNindStreamingReceipt, RawFoundationError> {
    let global_input_mean = packed
        .channels
        .iter()
        .flatten()
        .map(|value| f64::from(*value))
        .sum::<f64>()
        / (4 * packed.width * packed.height) as f64;
    let maximum_accumulator_rows = maximum_accumulator_rows(packed.height, packed.width);
    let first = run_streaming_pass(
        packed,
        model,
        cancellation,
        None,
        maximum_accumulator_rows,
        None,
    )?;
    if first.raw_mean.abs() <= 1e-6 * global_input_mean.abs() {
        return Err(RawFoundationError::ModelContract(
            "RawNIND output mean cannot be gain matched".into(),
        ));
    }
    let global_gain = global_input_mean / first.raw_mean;
    let second = run_streaming_pass(
        packed,
        model,
        cancellation,
        Some(global_gain),
        maximum_accumulator_rows,
        Some(sink),
    )?;
    Ok(RawNindStreamingReceipt {
        packed_width: packed.width,
        packed_height: packed.height,
        output_width: packed.width * 2,
        output_height: packed.height * 2,
        tile_inferences: first.tile_count + second.tile_count,
        global_input_mean,
        first_pass_raw_output_mean: first.raw_mean,
        second_pass_raw_output_mean: second.raw_mean,
        global_gain,
        output_mean: second.output_mean,
        maximum_accumulator_rows,
    })
}

struct StreamingPassReceipt {
    raw_mean: f64,
    output_mean: f64,
    tile_count: usize,
}

#[allow(clippy::too_many_arguments)]
fn run_streaming_pass(
    packed: &PackedBayer,
    model: &mut RawNindModel,
    cancellation: &CancellationToken,
    global_gain: Option<f64>,
    maximum_rows: usize,
    mut sink: Option<&mut dyn RawFoundationStripeSink>,
) -> Result<StreamingPassReceipt, RawFoundationError> {
    let rows = packed.height.div_ceil(STEP);
    let columns = packed.width.div_ceil(STEP);
    let output_height = packed.height * 2;
    let output_width = packed.width * 2;
    let accumulator_plane = maximum_rows * output_width;
    let mut accumulator = vec![0.0_f32; 3 * accumulator_plane];
    let mut coverage = vec![0.0_f32; accumulator_plane];
    let mut buffer_start = 0_usize;
    let mut active_end = 0_usize;
    let mut raw_sum = 0.0_f64;
    let mut output_sum = 0.0_f64;
    let mut value_count = 0_usize;
    let mut tile_count = 0_usize;
    for row in 0..rows {
        if cancellation.is_cancelled() {
            return Err(infer_artifact_lease::ArtifactLeaseError::Cancelled.into());
        }
        let row_end = placement(row, 0, output_height, output_width).global_y1;
        active_end = active_end.max(row_end);
        let weight_y = axis_weight(row > 0, row + 1 < rows);
        for column in 0..columns {
            if cancellation.is_cancelled() {
                return Err(infer_artifact_lease::ArtifactLeaseError::Cancelled.into());
            }
            let weight_x = axis_weight(column > 0, column + 1 < columns);
            let placement = placement(row, column, output_height, output_width);
            let input = extract_tile(packed, row, column);
            let (tile, _) = model.run_tile(input, &cancellation.child_token())?;
            tile_count += 1;
            let global_y0 = placement.global_y0.max(buffer_start);
            if placement.global_y1 <= global_y0 {
                continue;
            }
            let local_y0 = placement.local_y0 + global_y0 - placement.global_y0;
            let crop_height = placement.global_y1 - global_y0;
            let crop_width = placement.global_x1 - placement.global_x0;
            for local_y in 0..crop_height {
                let tile_y = local_y0 + local_y;
                let buffer_y = global_y0 + local_y - buffer_start;
                for local_x in 0..crop_width {
                    let tile_x = placement.local_x0 + local_x;
                    let global_x = placement.global_x0 + local_x;
                    let weight = weight_y[tile_y] * weight_x[tile_x];
                    let buffer_index = buffer_y * output_width + global_x;
                    coverage[buffer_index] += weight;
                    for channel in 0..3 {
                        let tile_index = channel * 1024 * 1024 + tile_y * 1024 + tile_x;
                        accumulator[channel * accumulator_plane + buffer_index] +=
                            tile[tile_index] * weight;
                    }
                }
            }
        }
        let finalize_end = stripe_end(row, rows, output_height);
        let emitted_rows = finalize_end - buffer_start;
        if emitted_rows == 0 || emitted_rows > maximum_rows {
            return Err(RawFoundationError::ModelContract(
                "RawNIND stripe plan exceeded its accumulator".into(),
            ));
        }
        for y in 0..emitted_rows {
            for x in 0..output_width {
                let index = y * output_width + x;
                let weight = coverage[index];
                if weight <= 0.0 {
                    return Err(RawFoundationError::ModelContract(
                        "RawNIND stripe contains uncovered pixels".into(),
                    ));
                }
                for channel in 0..3 {
                    let value = &mut accumulator[channel * accumulator_plane + index];
                    *value /= weight;
                    raw_sum += f64::from(*value);
                    if let Some(gain) = global_gain {
                        *value *= gain as f32;
                    }
                    output_sum += f64::from(*value);
                    value_count += 1;
                }
            }
        }
        if let Some(sink) = sink.as_deref_mut() {
            sink.write_stripe(
                buffer_start,
                emitted_rows,
                output_width,
                maximum_rows,
                &accumulator,
            )?;
        }
        let retained_rows = active_end - finalize_end;
        let previous_rows = active_end - buffer_start;
        for channel in 0..3 {
            let base = channel * accumulator_plane;
            accumulator.copy_within(
                base + emitted_rows * output_width..base + previous_rows * output_width,
                base,
            );
            accumulator[base + retained_rows * output_width..base + previous_rows * output_width]
                .fill(0.0);
        }
        coverage.copy_within(emitted_rows * output_width..previous_rows * output_width, 0);
        coverage[retained_rows * output_width..previous_rows * output_width].fill(0.0);
        buffer_start = finalize_end;
    }
    if buffer_start != output_height || value_count != 3 * output_height * output_width {
        return Err(RawFoundationError::ModelContract(
            "RawNIND stripe pass did not finalize the output".into(),
        ));
    }
    Ok(StreamingPassReceipt {
        raw_mean: raw_sum / value_count as f64,
        output_mean: output_sum / value_count as f64,
        tile_count,
    })
}

fn maximum_accumulator_rows(packed_height: usize, packed_width: usize) -> usize {
    let rows = packed_height.div_ceil(STEP);
    let output_height = packed_height * 2;
    let output_width = packed_width * 2;
    let mut buffer_start = 0;
    let mut maximum = 0;
    for row in 0..rows {
        let row_end = placement(row, 0, output_height, output_width).global_y1;
        maximum = maximum.max(row_end - buffer_start);
        buffer_start = stripe_end(row, rows, output_height);
    }
    maximum
}

fn stripe_end(row: usize, rows: usize, output_height: usize) -> usize {
    if row + 1 == rows {
        output_height
    } else {
        ((row + 1) * STEP - BLEND_OVERLAP + EXACT_HALO) * 2
    }
}

fn run_pass(
    packed: &PackedBayer,
    model: &mut RawNindModel,
    cancellation: &CancellationToken,
) -> Result<(Vec<f32>, usize), RawFoundationError> {
    let rows = packed.height.div_ceil(STEP);
    let columns = packed.width.div_ceil(STEP);
    let output_height = packed.height * OUTPUT_SCALE;
    let output_width = packed.width * OUTPUT_SCALE;
    let plane = output_height * output_width;
    let mut output = vec![0.0_f32; 3 * plane];
    let mut coverage = vec![0.0_f32; plane];
    let mut tile_count = 0;
    for row in 0..rows {
        let weight_y = axis_weight(row > 0, row + 1 < rows);
        for column in 0..columns {
            if cancellation.is_cancelled() {
                return Err(infer_artifact_lease::ArtifactLeaseError::Cancelled.into());
            }
            let weight_x = axis_weight(column > 0, column + 1 < columns);
            let placement = placement(row, column, output_height, output_width);
            let input = extract_tile(packed, row, column);
            let tile_cancellation = cancellation.child_token();
            let (tile, _) = model.run_tile(input, &tile_cancellation)?;
            tile_count += 1;
            let crop_height = placement.global_y1 - placement.global_y0;
            let crop_width = placement.global_x1 - placement.global_x0;
            for local_y in 0..crop_height {
                let tile_y = placement.local_y0 + local_y;
                let global_y = placement.global_y0 + local_y;
                for local_x in 0..crop_width {
                    let tile_x = placement.local_x0 + local_x;
                    let global_x = placement.global_x0 + local_x;
                    let weight = weight_y[tile_y] * weight_x[tile_x];
                    let global_index = global_y * output_width + global_x;
                    coverage[global_index] += weight;
                    for channel in 0..3 {
                        let tile_index = channel * 1024 * 1024 + tile_y * 1024 + tile_x;
                        output[channel * plane + global_index] += tile[tile_index] * weight;
                    }
                }
            }
        }
    }
    for (index, weight) in coverage.into_iter().enumerate() {
        if weight <= 0.0 {
            return Err(RawFoundationError::ModelContract(
                "tiling left uncovered output pixels".into(),
            ));
        }
        for channel in 0..3 {
            output[channel * plane + index] /= weight;
        }
    }
    Ok((output, tile_count))
}

fn axis_weight(has_previous: bool, has_next: bool) -> Vec<f32> {
    let edge = TILE_EDGE * OUTPUT_SCALE;
    let halo = EXACT_HALO * OUTPUT_SCALE;
    let blend_width = 2 * (BLEND_OVERLAP - EXACT_HALO) * OUTPUT_SCALE;
    let mut weight = vec![1.0_f32; edge];
    if has_previous {
        weight[..halo].fill(0.0);
        for index in 0..blend_width {
            weight[halo + index] = (index as f32 + 0.5) / blend_width as f32;
        }
    }
    if has_next {
        let start = edge - halo - blend_width;
        for index in 0..blend_width {
            weight[start + index] = (blend_width as f32 - index as f32 - 0.5) / blend_width as f32;
        }
        weight[edge - halo..].fill(0.0);
    }
    weight
}

fn placement(row: usize, column: usize, output_height: usize, output_width: usize) -> Placement {
    let origin_y = row as isize * STEP as isize - BLEND_OVERLAP as isize;
    let origin_x = column as isize * STEP as isize - BLEND_OVERLAP as isize;
    let global_y0 = (origin_y * 2).max(0) as usize;
    let global_x0 = (origin_x * 2).max(0) as usize;
    let global_y1 = ((origin_y + TILE_EDGE as isize) * 2).clamp(0, output_height as isize) as usize;
    let global_x1 = ((origin_x + TILE_EDGE as isize) * 2).clamp(0, output_width as isize) as usize;
    Placement {
        global_y0,
        global_y1,
        global_x0,
        global_x1,
        local_y0: (global_y0 as isize - origin_y * 2) as usize,
        local_x0: (global_x0 as isize - origin_x * 2) as usize,
    }
}

fn extract_tile(packed: &PackedBayer, row: usize, column: usize) -> Vec<f32> {
    let origin_y = row as isize * STEP as isize - BLEND_OVERLAP as isize;
    let origin_x = column as isize * STEP as isize - BLEND_OVERLAP as isize;
    let plane = TILE_EDGE * TILE_EDGE;
    let mut tile = vec![0.0_f32; 4 * plane];
    for channel in 0..4 {
        for y in 0..TILE_EDGE {
            let source_y = reflected(packed.height, origin_y + y as isize);
            for x in 0..TILE_EDGE {
                let source_x = reflected(packed.width, origin_x + x as isize);
                tile[channel * plane + y * TILE_EDGE + x] =
                    packed.channels[channel][source_y * packed.width + source_x];
            }
        }
    }
    tile
}

fn reflected(length: usize, coordinate: isize) -> usize {
    let period = (2 * length - 2) as isize;
    let folded = coordinate.rem_euclid(period);
    if folded < length as isize {
        folded as usize
    } else {
        (period - folded) as usize
    }
}

fn mean(values: &[f32]) -> f64 {
    values.iter().map(|value| f64::from(*value)).sum::<f64>() / values.len() as f64
}

fn sequence_digests(output: &[f32], height: usize, width: usize) -> (String, String) {
    let header = format!("{{\"format\":\"{STRIPE_FORMAT}\",\"shape\":[3,{height},{width}]}}");
    let mut sequence = Sha256::new();
    sequence.update(header.as_bytes());
    let mut payload = Sha256::new();
    let plane = height * width;
    let rows = height.div_ceil(STEP * 2);
    let mut y_start = 0;
    for row in 0..rows {
        let y_end = if row + 1 == rows {
            height
        } else {
            ((row + 1) * STEP - BLEND_OVERLAP + EXACT_HALO) * 2
        };
        for channel in 0..3 {
            for y in y_start..y_end {
                for x in 0..width {
                    let bytes = output[channel * plane + y * width + x].to_le_bytes();
                    sequence.update(bytes);
                    payload.update(bytes);
                }
            }
        }
        y_start = y_end;
    }
    (
        format!("{:x}", sequence.finalize()),
        format!("{:x}", payload.finalize()),
    )
}

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
};

use infer_artifact_lease::{ArtifactLeaseRegistry, ConsumedArtifactLease};
use sha2::{Digest, Sha256};

use crate::{RawFoundationError, RawFoundationRequest};

#[derive(Debug)]
pub struct PackedBayer {
    pub channels: [Vec<f32>; 4],
    pub height: usize,
    pub width: usize,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct StagingReceipt {
    pub sample_bytes: u64,
    pub decoded_samples_sha256: String,
    pub source_cfa: String,
    pub force_rggb_crop_sensor: [u8; 2],
    pub maximum_explicit_sample_buffer_bytes: usize,
    pub explicit_full_sample_buffers: usize,
}

pub fn consume_and_prepare_staging(
    registry: &ArtifactLeaseRegistry,
    request: &RawFoundationRequest,
    app_id: &str,
    job_id: &str,
    daemon_generation: &str,
    now_unix_ms: u64,
) -> Result<(PackedBayer, StagingReceipt, ConsumedArtifactLease), RawFoundationError> {
    request.validate()?;
    let mut lease = registry.consume(
        &request.lease_id,
        app_id,
        job_id,
        daemon_generation,
        now_unix_ms,
    )?;
    if lease.identity.input_size_bytes != request.staging.sample_bytes {
        return Err(RawFoundationError::SampleIdentityMismatch);
    }
    let mut input = File::from(lease.input);
    input.seek(SeekFrom::Start(0))?;
    let (packed, crop, actual, bytes_read, maximum_buffer) =
        pack_normalized_from_file(&mut input, request)?;
    if bytes_read != request.staging.sample_bytes
        || actual != request.staging.decoded_samples_sha256
    {
        return Err(RawFoundationError::SampleIdentityMismatch);
    }
    lease.input = input.into();
    Ok((
        packed,
        StagingReceipt {
            sample_bytes: bytes_read,
            decoded_samples_sha256: actual,
            source_cfa: request.staging.cfa.clone(),
            force_rggb_crop_sensor: crop,
            maximum_explicit_sample_buffer_bytes: maximum_buffer,
            explicit_full_sample_buffers: 0,
        },
        lease,
    ))
}

fn pack_normalized_from_file(
    input: &mut File,
    request: &RawFoundationRequest,
) -> Result<(PackedBayer, [u8; 2], String, u64, usize), RawFoundationError> {
    let source_width = request.staging.width as usize;
    let source_height = request.staging.height as usize;
    let cfa = request.staging.cfa.as_bytes();
    let red = cfa
        .iter()
        .position(|site| *site == b'R')
        .ok_or_else(|| RawFoundationError::InvalidRequest("CFA has no red site".into()))?;
    let row_offset = red / 2;
    let column_offset = red % 2;
    let cropped_width = source_width.saturating_sub(column_offset) & !1;
    let cropped_height = source_height.saturating_sub(row_offset) & !1;
    if cropped_width < 2 || cropped_height < 2 {
        return Err(RawFoundationError::InvalidRequest(
            "RGGB crop leaves no complete Bayer cell".into(),
        ));
    }
    let packed_width = cropped_width / 2;
    let packed_height = cropped_height / 2;
    let mut channels: [Vec<f32>; 4] =
        std::array::from_fn(|_| Vec::with_capacity(packed_width * packed_height));
    let mut hasher = Sha256::new();
    let mut bytes_read = 0_u64;
    let mut row = vec![0_u8; source_width * 2];
    for source_y in 0..source_height {
        input.read_exact(&mut row)?;
        hasher.update(&row);
        bytes_read = bytes_read.saturating_add(row.len() as u64);
        if source_y < row_offset || source_y >= row_offset + cropped_height {
            continue;
        }
        let local_y = (source_y - row_offset) % 2;
        for packed_x in 0..packed_width {
            for local_x in 0..2 {
                let channel = local_y * 2 + local_x;
                let source_x = column_offset + packed_x * 2 + local_x;
                let byte_index = source_x * 2;
                let sample = u16::from_le_bytes([row[byte_index], row[byte_index + 1]]);
                let source_site =
                    ((local_y + row_offset) % 2) * 2 + ((local_x + column_offset) % 2);
                let black = request.staging.black_levels[source_site] as f32;
                let white = request.staging.white_levels[source_site] as f32;
                channels[channel].push(((sample as f32 - black) / (white - black)).clamp(0.0, 1.0));
            }
        }
    }
    if channels
        .iter()
        .any(|channel| channel.len() != packed_width * packed_height)
    {
        return Err(RawFoundationError::InvalidRequest(
            "staging payload did not produce a complete packed Bayer image".into(),
        ));
    }
    Ok((
        PackedBayer {
            channels,
            height: packed_height,
            width: packed_width,
        },
        [row_offset as u8, column_offset as u8],
        format!("{:x}", hasher.finalize()),
        bytes_read,
        row.len(),
    ))
}

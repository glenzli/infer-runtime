use std::{
    fs::File,
    io::{Seek, SeekFrom, Write},
};

use infer_artifact_lease::ArtifactLeaseRegistry;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{
    RawFoundationError, RawFoundationRequest, RawFoundationStripeSink, RawNindModel,
    consume_and_prepare_staging, materialize_two_pass_streaming,
};

const FILE_MAGIC: &[u8; 8] = b"SHRAWF01";
const FOOTER_MAGIC: &[u8; 8] = b"SHRFEND1";
const ARTIFACT_SCHEMA: &str = "shadow-raw-foundation-artifact-v1";
const HEADER_SCHEMA: &str = "shadow-raw-foundation-header-v1";
const CACHE_SCHEMA: &str = "shadow-raw-foundation-cache-key-v1";

#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct RawFoundationArtifactReceipt {
    pub cache_key_sha256: String,
    pub artifact_identity_sha256: String,
    pub artifact_file_sha256: String,
    pub artifact_file_bytes: u64,
    pub sequence_sha256: String,
    pub payload_sha256: String,
    pub output_width: usize,
    pub output_height: usize,
    pub tile_inferences: usize,
    pub maximum_accumulator_rows: usize,
    pub explicit_full_output_buffers: usize,
    pub implementation_revision: String,
    pub cache_identity: String,
}

#[allow(clippy::too_many_arguments)]
pub fn materialize_foundation_to_lease(
    registry: &ArtifactLeaseRegistry,
    request: &RawFoundationRequest,
    app_id: &str,
    job_id: &str,
    daemon_generation: &str,
    now_unix_ms: u64,
    model: &mut RawNindModel,
    cancellation: &CancellationToken,
) -> Result<RawFoundationArtifactReceipt, RawFoundationError> {
    let (packed, staging, lease) = consume_and_prepare_staging(
        registry,
        request,
        app_id,
        job_id,
        daemon_generation,
        now_unix_ms,
    )?;
    let mut output = File::from(lease.output);
    let result = (|| {
        let contract = json!({
            "source_sha256": request.source.sha256,
            "source_size_bytes": request.source.size_bytes,
            "source_pixel_contract_sha256": request.source.pixel_contract_sha256,
            "raw_preprocessing": {
                "sensor_shape": [packed.height * 2, packed.width * 2],
                "packed_shape": [4, packed.height, packed.width],
                "raw_pattern": forced_pattern(staging.force_rggb_crop_sensor),
                "source_raw_pattern": [[0, 1], [2, 3]],
                "force_rggb_crop_sensor": staging.force_rggb_crop_sensor,
                "color_description": request.staging.cfa,
                "white_level": f64::from(request.staging.white_levels[0]),
                "black_level_per_channel": request.staging.black_levels.map(f64::from),
                "decoder_provider_id": request.staging.decoder_provider_id,
                "decoder_provider_version": request.staging.decoder_provider_version,
                "decoded_samples_sha256": staging.decoded_samples_sha256,
            },
            "model": {
                "graph_member": "rawdenoise-nind/model_bayer.onnx",
                "graph_sha256": model.identity().graph_sha256,
                "license": "GPL-3.0",
                "package_sha256": "d71b5f1e727c85a359e6f74dca9e2016c9d8fc3e2f7ac3e9b347d80ceca969af",
                "release": "release-5.6.0",
                "repository": "https://github.com/darktable-org/darktable-ai",
                "revision": "5454d7aa6d89a67054fd4a83343b09e69acaf76a",
                "training_repository": "https://github.com/trougnouf/rawnind_jddc",
                "training_revision": "4d455aa8ada69214eafa6a91ac0b2e011cf9dcb7",
            },
            "execution": {
                "engine": "onnxruntime",
                "runtime_version": model.identity().runtime,
                "requested_provider": "cpu",
                "active_providers": [model.identity().execution_provider],
                "platform": std::env::consts::OS,
                "machine": std::env::consts::ARCH,
            },
            "algorithm": {
                "blend_overlap_packed": 112,
                "blend_width_packed": 24,
                "exact_halo_packed": 100,
                "inference_passes": 2,
                "input_channel_order": ["R", "G1", "G2", "B"],
                "implementation_revision": model.identity().implementation_revision,
                "normalization": "per-cfa-site-black-to-white-range-clipped",
                "output_scale": 2,
                "output_space": "linear-camera-rgb",
                "padding": "numpy-reflect-direct-index",
                "pool_alignment_packed": 16,
                "scale_policy": "one-global-output-mean-to-input-mean",
                "step_packed": 288,
                "tile_edge_packed": 512,
                "white_balance": "none",
            }
        });
        let shape = [3, packed.height * 2, packed.width * 2];
        let cache_key_sha256 = canonical_sha256(&json!({
            "contract": contract,
            "output_shape_sensor": shape,
            "schema": CACHE_SCHEMA,
        }))?;
        let header = json!({
            "cache_key_sha256": cache_key_sha256,
            "contract": contract,
            "output": {
                "byte_order": "little",
                "channels": ["R", "G", "B"],
                "demosaiced": true,
                "dtype": "float32",
                "layout": "stripe-chw",
                "shape_sensor": shape,
                "space": "linear-camera-rgb",
            },
            "schema": HEADER_SCHEMA,
            "semantic_boundary": "raw-foundation-materialization",
        });
        let header_bytes = canonical_json(&header)?;
        let mut writer = HashingWriter::new(&mut output);
        writer.write_all(FILE_MAGIC)?;
        writer.write_all(&(header_bytes.len() as u64).to_le_bytes())?;
        writer.write_all(&header_bytes)?;
        let header_sha256 = format!("{:x}", Sha256::digest(&header_bytes));
        if cfg!(target_endian = "big") {
            return Err(RawFoundationError::ModelContract(
                "foundation artifact writer requires little endian".into(),
            ));
        }
        let mut sink = ArtifactStripeWriter::new(&mut writer, shape[1], shape[2]);
        let materialized = materialize_two_pass_streaming(&packed, model, cancellation, &mut sink)?;
        let sink = sink.finish()?;
        let publication = json!({
            "first_pass_raw_output_mean_f64_bits": f64_bits(materialized.first_pass_raw_output_mean),
            "global_gain_f64_bits": f64_bits(materialized.global_gain),
            "global_input_mean_f64_bits": f64_bits(materialized.global_input_mean),
            "output_mean_f64_bits": f64_bits(materialized.output_mean),
            "producer": "rawnind-bayer-two-pass-stripe-v1",
            "replay_relative_mean_delta_f64_bits": f64_bits(replay_delta(materialized.first_pass_raw_output_mean, materialized.second_pass_raw_output_mean)),
            "second_pass_raw_output_mean_f64_bits": f64_bits(materialized.second_pass_raw_output_mean),
        });
        let mut manifest = json!({
            "artifact_identity_sha256": "",
            "cache_key_sha256": cache_key_sha256,
            "header_sha256": header_sha256,
            "payload_bytes": (3 * materialized.output_height * materialized.output_width * 4) as u64,
            "publication": publication,
            "schema": ARTIFACT_SCHEMA,
            "sequence_sha256": sink.sequence_sha256,
            "stripes": sink.stripes,
        });
        let identity_material = manifest
            .as_object()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.as_str() != "artifact_identity_sha256")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<serde_json::Map<_, _>>();
        let artifact_identity_sha256 = canonical_sha256(&Value::Object(identity_material))?;
        manifest["artifact_identity_sha256"] = artifact_identity_sha256.clone().into();
        let manifest_bytes = canonical_json(&manifest)?;
        let manifest_offset = writer.bytes_written();
        writer.write_all(&manifest_bytes)?;
        writer.write_all(FOOTER_MAGIC)?;
        writer.write_all(&manifest_offset.to_le_bytes())?;
        writer.write_all(&(manifest_bytes.len() as u64).to_le_bytes())?;
        writer.write_all(&Sha256::digest(&manifest_bytes))?;
        let artifact_file_bytes = writer.bytes_written();
        let artifact_file_sha256 = writer.finish();
        output.sync_all()?;
        Ok(RawFoundationArtifactReceipt {
            cache_key_sha256,
            artifact_identity_sha256,
            artifact_file_sha256,
            artifact_file_bytes,
            sequence_sha256: sink.sequence_sha256,
            payload_sha256: sink.payload_sha256,
            output_width: materialized.output_width,
            output_height: materialized.output_height,
            tile_inferences: materialized.tile_inferences,
            maximum_accumulator_rows: materialized.maximum_accumulator_rows,
            explicit_full_output_buffers: 0,
            implementation_revision: model.identity().implementation_revision.clone(),
            cache_identity: model.identity().cache_identity.clone(),
        })
    })();
    if result.is_err() {
        let _ = output.set_len(0);
        let _ = output.seek(SeekFrom::Start(0));
        let _ = output.sync_all();
    }
    result
}

struct ArtifactStripeWriter<'writer, 'file> {
    writer: &'writer mut HashingWriter<'file>,
    height: usize,
    width: usize,
    next_y: usize,
    sequence: Sha256,
    payload: Sha256,
    stripes: Vec<Value>,
}

struct FinishedStripes {
    sequence_sha256: String,
    payload_sha256: String,
    stripes: Vec<Value>,
}

impl<'writer, 'file> ArtifactStripeWriter<'writer, 'file> {
    fn new(writer: &'writer mut HashingWriter<'file>, height: usize, width: usize) -> Self {
        let mut sequence = Sha256::new();
        sequence.update(
            format!(
                "{{\"format\":\"shadow-linear-camera-rgb-f32-stripe-chw-v1\",\"shape\":[3,{height},{width}]}}"
            )
            .as_bytes(),
        );
        Self {
            writer,
            height,
            width,
            next_y: 0,
            sequence,
            payload: Sha256::new(),
            stripes: Vec::new(),
        }
    }

    fn finish(self) -> Result<FinishedStripes, RawFoundationError> {
        if self.next_y != self.height {
            return Err(RawFoundationError::ModelContract(
                "foundation artifact stripe output is incomplete".into(),
            ));
        }
        Ok(FinishedStripes {
            sequence_sha256: format!("{:x}", self.sequence.finalize()),
            payload_sha256: format!("{:x}", self.payload.finalize()),
            stripes: self.stripes,
        })
    }
}

impl RawFoundationStripeSink for ArtifactStripeWriter<'_, '_> {
    fn write_stripe(
        &mut self,
        y_start: usize,
        rows: usize,
        width: usize,
        channel_stride_rows: usize,
        values: &[f32],
    ) -> Result<(), RawFoundationError> {
        if y_start != self.next_y || width != self.width || y_start + rows > self.height {
            return Err(RawFoundationError::ModelContract(
                "foundation artifact stripe order or shape changed".into(),
            ));
        }
        let offset = self.writer.bytes_written();
        let mut stripe_hasher = Sha256::new();
        let plane = channel_stride_rows * width;
        for channel in 0..3 {
            for y in 0..rows {
                let start = channel * plane + y * width;
                let row = &values[start..start + width];
                // SAFETY: f32 has no invalid bit patterns; the borrowed row
                // outlives this call and big-endian hosts are rejected by the
                // owning artifact writer before execution begins.
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        row.as_ptr().cast::<u8>(),
                        std::mem::size_of_val(row),
                    )
                };
                self.writer.write_all(bytes)?;
                self.sequence.update(bytes);
                self.payload.update(bytes);
                stripe_hasher.update(bytes);
            }
        }
        let byte_length = self.writer.bytes_written() - offset;
        self.stripes.push(json!({
            "index": self.stripes.len(),
            "y_start": y_start,
            "rows": rows,
            "offset": offset,
            "byte_length": byte_length,
            "sha256": format!("{:x}", stripe_hasher.finalize()),
        }));
        self.next_y += rows;
        Ok(())
    }
}

fn forced_pattern(crop: [u8; 2]) -> [[u8; 2]; 2] {
    let source = [[0_u8, 1], [2, 3]];
    std::array::from_fn(|row| {
        std::array::from_fn(|column| {
            source[(row + crop[0] as usize) % 2][(column + crop[1] as usize) % 2]
        })
    })
}

fn replay_delta(first: f64, second: f64) -> f64 {
    let scale = 0.5 * (first.abs() + second.abs());
    if scale == 0.0 {
        0.0
    } else {
        (first - second).abs() / scale
    }
}

fn f64_bits(value: f64) -> String {
    format!("{:016x}", value.to_bits())
}

fn canonical_json(value: &Value) -> Result<Vec<u8>, RawFoundationError> {
    serde_json::to_vec(value).map_err(|error| RawFoundationError::ModelContract(error.to_string()))
}

fn canonical_sha256(value: &Value) -> Result<String, RawFoundationError> {
    Ok(format!("{:x}", Sha256::digest(canonical_json(value)?)))
}

struct HashingWriter<'a> {
    file: &'a mut File,
    hasher: Sha256,
    bytes: u64,
}

impl<'a> HashingWriter<'a> {
    fn new(file: &'a mut File) -> Self {
        Self {
            file,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    fn bytes_written(&self) -> u64 {
        self.bytes
    }

    fn finish(self) -> String {
        format!("{:x}", self.hasher.finalize())
    }
}

impl Write for HashingWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.file.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

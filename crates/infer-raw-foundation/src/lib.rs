//! Disabled-by-default typed RawNIND foundation contract and adapter.
//!
//! The daemon mounts this data plane only when the exact experimental Build is
//! enabled and validated. This owner freezes the small control request, binds
//! it to an already registered artifact lease, validates and hashes the sample
//! descriptor, and owns the immutable RawNIND tensor contract. Shadow
//! continues to own source decoding, cache lookup, artifact verification,
//! publication, and stale-result arbitration.

mod artifact;
mod contract;
mod model;
mod staging;
mod tiling;

pub use contract::{
    RAW_FOUNDATION_CONTRACT, RAW_FOUNDATION_ENDPOINT, RAW_FOUNDATION_INTENT,
    RAW_FOUNDATION_LEASE_ENDPOINT, RAW_FOUNDATION_STAGING_SCHEMA, RawFoundationExecuteRequest,
    RawFoundationLeaseRequest, RawFoundationPriority, RawFoundationRequest, RawFoundationSource,
    RawFoundationStagingDescriptor,
};
pub use model::{RawNindModel, RawNindModelIdentity, RawNindTileReceipt};
pub use staging::{PackedBayer, StagingReceipt, consume_and_prepare_staging};
pub use tiling::{
    RawFoundationStripeSink, RawNindMaterializationReceipt, RawNindStreamingReceipt,
    materialize_two_pass, materialize_two_pass_streaming,
};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RawFoundationError {
    #[error("raw foundation request is invalid: {0}")]
    InvalidRequest(String),
    #[error("raw foundation sample payload does not match its descriptor")]
    SampleIdentityMismatch,
    #[error("raw foundation artifact lease failed")]
    Lease(#[from] infer_artifact_lease::ArtifactLeaseError),
    #[error("raw foundation I/O failed")]
    Io(#[from] std::io::Error),
    #[error("RawNIND model identity or tensor contract changed: {0}")]
    ModelContract(String),
    #[error("RawNIND native execution failed: {0}")]
    NativeExecution(String),
}

#[cfg(all(test, unix))]
mod real_tests {
    use std::{
        fs::{self, File, OpenOptions},
        io::{Read, Seek, SeekFrom},
        os::{fd::AsRawFd, unix::fs::PermissionsExt},
        path::PathBuf,
        sync::Arc,
        time::Instant,
    };

    use infer_artifact_lease::{
        ArtifactLeaseRegistry, ArtifactLeaseUnixServer, register_unix_handles,
    };
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;

    #[test]
    #[ignore = "requires pinned local RawNIND graph, ORT 1.27, and external synthetic evidence"]
    fn experimental_ort127_fixture_reports_pixel_and_sequence_parity() {
        let graph = required_path("INFER_TEST_RAWNIND_GRAPH");
        let library = required_path("INFER_TEST_ORT_LIBRARY");
        let sample_source = required_path("INFER_TEST_RAWNIND_SAMPLE");
        let legacy_artifact = required_path("INFER_TEST_RAWNIND_LEGACY_ARTIFACT");
        RawNindModel::initialize_runtime(&library).unwrap();
        let temp = TempDir::new().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let input_path = temp.path().join("samples.u16le");
        let output_path = temp.path().join("output.shadowrawf");
        fs::copy(&sample_source, &input_path).unwrap();
        fs::set_permissions(&input_path, fs::Permissions::from_mode(0o600)).unwrap();
        let input = File::open(&input_path).unwrap();
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output_path)
            .unwrap();
        fs::set_permissions(&output_path, fs::Permissions::from_mode(0o600)).unwrap();
        let registry =
            Arc::new(ArtifactLeaseRegistry::new(unsafe { libc::geteuid() }, "gen_test").unwrap());
        let ticket = registry
            .issue_ticket("shadow", "job_test", "gen_test", 60_000, 1_000)
            .unwrap();
        let socket_path = temp.path().join("lease.sock");
        let server = ArtifactLeaseUnixServer::bind(&socket_path, Arc::clone(&registry)).unwrap();
        let server_thread = std::thread::spawn(move || server.accept_once(1_001).unwrap());
        let (lease_id, _) = register_unix_handles(
            &socket_path,
            &ticket.ticket_id,
            input.as_raw_fd(),
            output.as_raw_fd(),
        )
        .unwrap();
        server_thread.join().unwrap();
        let sample_bytes = fs::read(&input_path).unwrap();
        let request = RawFoundationRequest {
            model: RAW_FOUNDATION_INTENT.into(),
            lease_id,
            source_revision: "synthetic-phase0-v1".into(),
            source: RawFoundationSource {
                sha256: "a".repeat(64),
                size_bytes: 1,
                pixel_contract_sha256:
                    "e1998069001c14d01251cc3d6e2bc2aa66b807f3f17d246e7ee7270528302f7f".into(),
            },
            staging: RawFoundationStagingDescriptor {
                schema: RAW_FOUNDATION_STAGING_SCHEMA.into(),
                width: 2048,
                height: 2048,
                cfa: "RGGB".into(),
                black_levels: [64; 4],
                white_levels: [16_383; 4],
                sample_format: "uint16-le-row-major-active-bayer".into(),
                sample_bytes: sample_bytes.len() as u64,
                decoded_samples_sha256: format!("{:x}", Sha256::digest(&sample_bytes)),
                decoder_provider_id: "shadow.synthetic".into(),
                decoder_provider_version: "phase-0-v1".into(),
            },
        };
        drop(sample_bytes);
        let mut model = RawNindModel::open_experimental_cpu(&graph, "1.27.0").unwrap();
        let started = Instant::now();
        let result = materialize_foundation_to_lease(
            &registry,
            &request,
            "shadow",
            "job_test",
            "gen_test",
            1_002,
            &mut model,
            &CancellationToken::new(),
        )
        .unwrap();
        let elapsed = started.elapsed().as_secs_f64();
        let legacy = read_legacy_output(&legacy_artifact);
        let actual = read_legacy_output(&output_path);
        assert_portable_cache_key(&output_path);
        assert_eq!(legacy.len(), actual.len());
        let mut max_abs = 0.0_f32;
        let mut squared = 0.0_f64;
        for (actual, expected) in actual.iter().zip(&legacy) {
            let delta = (*actual - *expected).abs();
            max_abs = max_abs.max(delta);
            squared += f64::from(delta) * f64::from(delta);
        }
        let rmse = (squared / actual.len() as f64).sqrt();
        println!(
            "rawnind_ort127 wall_seconds={elapsed:.3} sequence_sha256={} payload_sha256={} cache_key_sha256={} artifact_identity_sha256={} artifact_file_sha256={} artifact_file_bytes={} max_abs={max_abs:.9} rmse={rmse:.12} tile_inferences={} maximum_accumulator_rows={} explicit_full_output_buffers={} implementation_revision={} cache_identity={}",
            result.sequence_sha256,
            result.payload_sha256,
            result.cache_key_sha256,
            result.artifact_identity_sha256,
            result.artifact_file_sha256,
            result.artifact_file_bytes,
            result.tile_inferences,
            result.maximum_accumulator_rows,
            result.explicit_full_output_buffers,
            model.identity().implementation_revision,
            model.identity().cache_identity,
        );
        assert!(result.maximum_accumulator_rows < 2048);
        assert_eq!(result.explicit_full_output_buffers, 0);
    }

    #[test]
    #[ignore = "requires pinned local RawNIND graph and ORT 1.27"]
    fn experimental_ort127_inflight_cancel_is_bounded() {
        let graph = required_path("INFER_TEST_RAWNIND_GRAPH");
        let library = required_path("INFER_TEST_ORT_LIBRARY");
        RawNindModel::initialize_runtime(&library).unwrap();
        let mut model = RawNindModel::open_experimental_cpu(&graph, "1.27.0").unwrap();
        let cancellation = CancellationToken::new();
        let cancellation_for_timer = cancellation.clone();
        let timer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(25));
            let started = Instant::now();
            cancellation_for_timer.cancel();
            started
        });
        let result = model.run_tile(vec![0.25_f32; 4 * 512 * 512], &cancellation);
        let cancelled_at = timer.join().unwrap();
        let stop_latency = cancelled_at.elapsed().as_secs_f64();
        assert!(matches!(
            result,
            Err(RawFoundationError::Lease(
                infer_artifact_lease::ArtifactLeaseError::Cancelled
            ))
        ));
        assert!(
            stop_latency <= 0.250,
            "cancel stop latency was {stop_latency}"
        );
        println!("rawnind_ort127_cancel stop_latency_seconds={stop_latency:.6}");
    }

    fn required_path(name: &str) -> PathBuf {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute() && path.is_file())
            .unwrap_or_else(|| panic!("{name} must name an existing absolute file"))
    }

    fn read_legacy_output(path: &PathBuf) -> Vec<f32> {
        let mut file = File::open(path).unwrap();
        let mut prefix = [0_u8; 16];
        file.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix[..8], b"SHRAWF01");
        let header_len = u64::from_le_bytes(prefix[8..].try_into().unwrap());
        file.seek(SeekFrom::End(-56)).unwrap();
        let mut footer = [0_u8; 56];
        file.read_exact(&mut footer).unwrap();
        assert_eq!(&footer[..8], b"SHRFEND1");
        let manifest_offset = u64::from_le_bytes(footer[8..16].try_into().unwrap());
        let manifest_len = u64::from_le_bytes(footer[16..24].try_into().unwrap());
        file.seek(SeekFrom::Start(manifest_offset)).unwrap();
        let mut manifest = vec![0_u8; manifest_len as usize];
        file.read_exact(&mut manifest).unwrap();
        let manifest: Value = serde_json::from_slice(&manifest).unwrap();
        let stripes = manifest["stripes"].as_array().unwrap();
        let shape = [3_usize, 2048, 2048];
        let plane = shape[1] * shape[2];
        let mut output = vec![0.0_f32; shape[0] * plane];
        for stripe in stripes {
            let y_start = stripe["y_start"].as_u64().unwrap() as usize;
            let rows = stripe["rows"].as_u64().unwrap() as usize;
            let offset = stripe["offset"].as_u64().unwrap();
            file.seek(SeekFrom::Start(offset)).unwrap();
            for channel in 0..3 {
                for y in y_start..y_start + rows {
                    let range =
                        channel * plane + y * shape[2]..channel * plane + (y + 1) * shape[2];
                    let mut bytes = vec![0_u8; shape[2] * 4];
                    file.read_exact(&mut bytes).unwrap();
                    for (target, value) in output[range].iter_mut().zip(bytes.chunks_exact(4)) {
                        *target = f32::from_le_bytes(value.try_into().unwrap());
                    }
                }
            }
        }
        assert!(header_len > 0);
        output
    }

    fn assert_portable_cache_key(path: &PathBuf) {
        let mut file = File::open(path).unwrap();
        let mut prefix = [0_u8; 16];
        file.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix[..8], b"SHRAWF01");
        let header_len = u64::from_le_bytes(prefix[8..].try_into().unwrap()) as usize;
        let mut header_bytes = vec![0_u8; header_len];
        file.read_exact(&mut header_bytes).unwrap();
        let header: Value = serde_json::from_slice(&header_bytes).unwrap();
        let preprocessing = &header["contract"]["raw_preprocessing"];
        assert!(
            std::str::from_utf8(&header_bytes)
                .unwrap()
                .contains("\"white_level\":16383.0")
        );
        assert_eq!(preprocessing["white_level"].as_f64(), Some(16_383.0));
        assert_eq!(
            preprocessing["black_level_per_channel"]
                .as_array()
                .unwrap()
                .iter()
                .map(Value::as_f64)
                .collect::<Option<Vec<_>>>()
                .unwrap(),
            vec![64.0; 4]
        );
        let identity = serde_json::json!({
            "contract": header["contract"].clone(),
            "output_shape_sensor": header["output"]["shape_sensor"].clone(),
            "schema": "shadow-raw-foundation-cache-key-v1",
        });
        let computed = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&identity).unwrap())
        );
        assert_eq!(header["cache_key_sha256"].as_str(), Some(computed.as_str()));
    }
}
pub use artifact::{RawFoundationArtifactReceipt, materialize_foundation_to_lease};

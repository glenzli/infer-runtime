//! Explicit, operator-initiated native reload measurement.
//!
//! A measurement owns only the timing loop and its output record. Resource
//! Manager owns admission, lifecycle exclusivity, native endpoint selection,
//! and the final inventory refresh.

use std::time::Instant;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{NativeControlError, NativeModelController};

pub const MAX_RELOAD_BENCHMARK_SAMPLES: u8 = 5;

#[derive(Debug, Clone, Deserialize)]
pub struct ReloadBenchmarkRequest {
    /// Each sample starts from an unloaded model and measures only the native
    /// load call. A small upper bound protects a local machine from an
    /// accidentally long benchmark run.
    pub samples: u8,
    /// Required operator-provided provenance, such as host conditions and the
    /// command purpose. It becomes part of the copyable config record.
    pub evidence: String,
}

#[derive(Debug, Error)]
pub enum ReloadBenchmarkRequestError {
    #[error("reload benchmark samples must be between 1 and {MAX_RELOAD_BENCHMARK_SAMPLES}")]
    InvalidSamples,
    #[error("reload benchmark evidence is required")]
    MissingEvidence,
}

impl ReloadBenchmarkRequest {
    pub fn validate(&self) -> Result<(), ReloadBenchmarkRequestError> {
        if self.samples == 0 || self.samples > MAX_RELOAD_BENCHMARK_SAMPLES {
            return Err(ReloadBenchmarkRequestError::InvalidSamples);
        }
        if self.evidence.trim().is_empty() {
            return Err(ReloadBenchmarkRequestError::MissingEvidence);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ReloadBenchmarkMeasurement {
    pub samples_ms: Vec<u64>,
    /// For an even sample count, this intentionally selects the upper median,
    /// avoiding a fabricated fractional duration.
    pub reload_cost_ms: u64,
    pub observed_at_unix_ms: u64,
    pub evidence: String,
    /// A version-controlled registry remains the source of truth, so the
    /// daemon emits rather than writes this TOML fragment.
    pub config_toml: String,
}

pub(super) async fn measure_reload(
    controller: &dyn NativeModelController,
    model: &str,
    request: &ReloadBenchmarkRequest,
    observed_at_unix_ms: u64,
    deployment: &str,
) -> Result<ReloadBenchmarkMeasurement, NativeControlError> {
    let mut samples_ms = Vec::with_capacity(usize::from(request.samples));
    for _ in 0..request.samples {
        let started = Instant::now();
        controller.load(model).await?;
        let elapsed_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        // Each subsequent sample must begin from a non-resident model. If
        // this fails, Resource Manager marks native state failed and refreshes
        // it rather than pretending that it is safe to continue.
        controller.unload(model).await?;
        samples_ms.push(elapsed_ms);
    }
    Ok(finalize_measurement(
        deployment,
        samples_ms,
        observed_at_unix_ms,
        request.evidence.trim(),
    ))
}

fn finalize_measurement(
    deployment: &str,
    mut samples_ms: Vec<u64>,
    observed_at_unix_ms: u64,
    evidence: &str,
) -> ReloadBenchmarkMeasurement {
    samples_ms.sort_unstable();
    let reload_cost_ms = samples_ms[samples_ms.len() / 2];
    let deployment_key = serde_json::to_string(deployment).expect("string is serializable");
    let evidence_value = serde_json::to_string(evidence).expect("string is serializable");
    let config_toml = format!(
        "[resources.reload_benchmarks.{deployment_key}]\nreload_cost_ms = {reload_cost_ms}\nobserved_at_unix_ms = {observed_at_unix_ms}\nevidence = {evidence_value}\n"
    );
    ReloadBenchmarkMeasurement {
        samples_ms,
        reload_cost_ms,
        observed_at_unix_ms,
        evidence: evidence.into(),
        config_toml,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_requires_bounded_samples_and_evidence() {
        assert!(
            ReloadBenchmarkRequest {
                samples: 0,
                evidence: "observed locally".into(),
            }
            .validate()
            .is_err()
        );
        assert!(
            ReloadBenchmarkRequest {
                samples: MAX_RELOAD_BENCHMARK_SAMPLES + 1,
                evidence: "observed locally".into(),
            }
            .validate()
            .is_err()
        );
        assert!(
            ReloadBenchmarkRequest {
                samples: 1,
                evidence: " ".into(),
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn record_uses_a_deterministic_upper_median_and_copyable_toml() {
        let measurement = finalize_measurement("local.2b", vec![12, 5, 9, 7], 42, "cold load");
        assert_eq!(measurement.samples_ms, vec![5, 7, 9, 12]);
        assert_eq!(measurement.reload_cost_ms, 9);
        assert!(
            measurement
                .config_toml
                .contains("[resources.reload_benchmarks.\"local.2b\"]")
        );
        assert!(measurement.config_toml.contains("evidence = \"cold load\""));
    }
}

//! Stable task contract for bounded, temporal, multi-label sound-event detection.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    ContractError, RequestConstraints,
    audio::{AudioFile, validate_model},
    string_enum,
};

#[derive(Debug, Clone)]
pub struct EventDetectionRequest {
    /// Stable task intent. Physical model identity remains in the selected Build.
    pub model: String,
    pub file: AudioFile,
    pub metadata: BTreeMap<String, String>,
}

impl EventDetectionRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_model(&self.model)?;
        self.file.validate()?;
        self.constraints().map(|_| ())
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        RequestConstraints::from_metadata(&self.metadata)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DetectedSoundEvent {
    /// Stable AudioSet ontology MID, not a model output index.
    pub class_id: String,
    pub label: String,
    pub start_seconds: f64,
    pub end_seconds: f64,
    /// Raw model sigmoid score after the versioned temporal smoothing policy.
    pub score: f64,
}

string_enum!(SpeechPresenceStatus {
    Present => "present",
    Absent => "absent",
    Unknown => "unknown"
});

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechPresence {
    pub status: SpeechPresenceStatus,
    /// Maximum smoothed score across the versioned speech-family class set.
    pub max_score: f64,
}

string_enum!(AudioCoverageStatus {
    Full => "full",
    Partial => "partial",
    None => "none"
});

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AudioAnalysisCoverage {
    pub status: AudioCoverageStatus,
    pub input_duration_seconds: f64,
    pub analyzed_start_seconds: f64,
    pub analyzed_end_seconds: f64,
    pub analyzed_seconds: f64,
    pub ratio: f64,
    pub window_count: usize,
    pub window_seconds: f64,
    pub hop_seconds: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SoundEventOntology {
    pub id: String,
    pub revision: String,
    pub class_id_namespace: String,
    pub class_count: usize,
    pub artifact_sha256: String,
    pub license_spdx: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SoundEventSmoothingPolicy {
    pub method: String,
    pub window_frames: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SoundEventDetectionPolicy {
    pub revision: String,
    pub score_kind: String,
    pub event_score_threshold: f64,
    pub smoothing: SoundEventSmoothingPolicy,
    pub max_classes_per_window: usize,
    pub max_events: usize,
    pub speech_class_set_revision: String,
    pub speech_present_threshold: f64,
    pub speech_absent_threshold: f64,
    pub max_audio_seconds: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SoundEventProvenance {
    pub model: String,
    pub model_archive_sha256: String,
    pub artifact_set_sha256: String,
    pub model_license_spdx: String,
    pub training_data_license_spdx: String,
    pub runtime: String,
    pub runtime_version: String,
    pub decoder: String,
    pub decoder_version: String,
    pub preprocessing_identity: String,
}

/// Provider-owned result before the control plane adds Job and logical-model identity.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventDetectionResult {
    pub object: String,
    pub events: Vec<DetectedSoundEvent>,
    pub speech_presence: SpeechPresence,
    pub coverage: AudioAnalysisCoverage,
    pub ontology: SoundEventOntology,
    pub policy: SoundEventDetectionPolicy,
    pub provenance: SoundEventProvenance,
}

impl EventDetectionResult {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.object != "audio.event_detection"
            || self.ontology.id.trim().is_empty()
            || self.ontology.revision.trim().is_empty()
            || self.ontology.class_id_namespace.trim().is_empty()
            || self.ontology.class_count == 0
            || !valid_sha256(&self.ontology.artifact_sha256)
            || self.ontology.license_spdx.trim().is_empty()
            || self.policy.revision.trim().is_empty()
            || self.policy.score_kind != "raw_sigmoid"
            || !unit_interval(self.policy.event_score_threshold)
            || !unit_interval(self.policy.speech_present_threshold)
            || !unit_interval(self.policy.speech_absent_threshold)
            || self.policy.speech_absent_threshold >= self.policy.speech_present_threshold
            || self.policy.smoothing.method.trim().is_empty()
            || self.policy.smoothing.window_frames == 0
            || self.policy.smoothing.window_frames.is_multiple_of(2)
            || self.policy.max_classes_per_window == 0
            || self.policy.max_events == 0
            || self.policy.speech_class_set_revision.trim().is_empty()
            || !unit_interval(self.speech_presence.max_score)
            || !finite_non_negative(self.coverage.input_duration_seconds)
            || self.coverage.input_duration_seconds == 0.0
            || !finite_non_negative(self.coverage.analyzed_start_seconds)
            || !finite_non_negative(self.coverage.analyzed_end_seconds)
            || !finite_non_negative(self.coverage.analyzed_seconds)
            || !unit_interval(self.coverage.ratio)
            || !finite_positive(self.coverage.window_seconds)
            || !finite_positive(self.coverage.hop_seconds)
            || self.coverage.analyzed_start_seconds > self.coverage.analyzed_end_seconds
            || self.coverage.analyzed_end_seconds > self.coverage.input_duration_seconds + 1e-6
            || self.coverage.analyzed_seconds > self.coverage.input_duration_seconds + 1e-6
            || self.coverage.input_duration_seconds > self.policy.max_audio_seconds as f64 + 1e-6
            || (self.coverage.analyzed_seconds
                - (self.coverage.analyzed_end_seconds - self.coverage.analyzed_start_seconds))
                .abs()
                > 1e-6
            || (self.coverage.ratio
                - self.coverage.analyzed_seconds / self.coverage.input_duration_seconds)
                .abs()
                > 1e-6
            || self.provenance.model.trim().is_empty()
            || !valid_sha256(&self.provenance.model_archive_sha256)
            || !valid_sha256(&self.provenance.artifact_set_sha256)
            || self.provenance.model_license_spdx.trim().is_empty()
            || self.provenance.training_data_license_spdx.trim().is_empty()
            || self.provenance.runtime.trim().is_empty()
            || self.provenance.runtime_version.trim().is_empty()
            || self.provenance.decoder.trim().is_empty()
            || self.provenance.decoder_version.trim().is_empty()
            || self.provenance.preprocessing_identity.trim().is_empty()
        {
            return Err(ContractError::InvalidAudio(
                "sound event result has invalid coverage, policy, ontology, or provenance".into(),
            ));
        }
        if self.coverage.status == AudioCoverageStatus::Full
            && (self.coverage.window_count == 0
                || (self.coverage.ratio - 1.0).abs() > 1e-6
                || self.coverage.analyzed_start_seconds.abs() > 1e-6
                || (self.coverage.analyzed_end_seconds - self.coverage.input_duration_seconds)
                    .abs()
                    > 1e-6)
        {
            return Err(ContractError::InvalidAudio(
                "full sound-event coverage must span the complete input".into(),
            ));
        }
        if self.coverage.status == AudioCoverageStatus::Partial
            && (self.coverage.window_count == 0
                || self.coverage.ratio <= 0.0
                || self.coverage.ratio >= 1.0
                || self.coverage.analyzed_seconds <= 0.0)
        {
            return Err(ContractError::InvalidAudio(
                "partial sound-event coverage must describe analyzed input".into(),
            ));
        }
        if self.coverage.status == AudioCoverageStatus::None
            && (self.coverage.window_count != 0
                || self.coverage.ratio != 0.0
                || self.coverage.analyzed_seconds != 0.0
                || !self.events.is_empty()
                || self.speech_presence.status != SpeechPresenceStatus::Unknown)
        {
            return Err(ContractError::InvalidAudio(
                "zero coverage cannot claim event or speech evidence".into(),
            ));
        }
        match self.speech_presence.status {
            SpeechPresenceStatus::Present
                if self.speech_presence.max_score < self.policy.speech_present_threshold =>
            {
                return Err(ContractError::InvalidAudio(
                    "present speech evidence is below the policy threshold".into(),
                ));
            }
            SpeechPresenceStatus::Absent
                if self.coverage.status != AudioCoverageStatus::Full
                    || self.speech_presence.max_score > self.policy.speech_absent_threshold =>
            {
                return Err(ContractError::InvalidAudio(
                    "absent speech requires full coverage and low model evidence".into(),
                ));
            }
            SpeechPresenceStatus::Unknown
                if self.speech_presence.max_score >= self.policy.speech_present_threshold
                    || (self.coverage.status == AudioCoverageStatus::Full
                        && self.speech_presence.max_score
                            <= self.policy.speech_absent_threshold) =>
            {
                return Err(ContractError::InvalidAudio(
                    "unknown speech evidence is inconsistent with coverage and thresholds".into(),
                ));
            }
            _ => {}
        }
        let mut previous_start = 0.0;
        for (index, event) in self.events.iter().enumerate() {
            if !event.class_id.starts_with("/m/")
                || event.label.trim().is_empty()
                || !finite_non_negative(event.start_seconds)
                || !finite_non_negative(event.end_seconds)
                || event.start_seconds < self.coverage.analyzed_start_seconds
                || event.end_seconds < event.start_seconds
                || (event.end_seconds - event.start_seconds).abs() <= f64::EPSILON
                || event.end_seconds > self.coverage.analyzed_end_seconds + 1e-6
                || !unit_interval(event.score)
                || event.score < self.policy.event_score_threshold
                || (index > 0 && event.start_seconds < previous_start)
            {
                return Err(ContractError::InvalidAudio(
                    "sound events must be ordered, bounded ontology intervals with unit scores"
                        .into(),
                ));
            }
            previous_start = event.start_seconds;
        }
        if self.events.len() > self.policy.max_events {
            return Err(ContractError::InvalidAudio(
                "sound-event result exceeds its versioned event-count bound".into(),
            ));
        }
        Ok(())
    }
}

fn unit_interval(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn finite_non_negative(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

fn finite_positive(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::MAX_AUDIO_UPLOAD_BYTES;

    #[test]
    fn event_detection_keeps_the_shared_twenty_five_mib_file_boundary() {
        let request = EventDetectionRequest {
            model: "audio.detect_events".into(),
            file: AudioFile {
                filename: "oversized.wav".into(),
                content_type: Some("audio/wav".into()),
                bytes: vec![0; MAX_AUDIO_UPLOAD_BYTES + 1],
            },
            metadata: BTreeMap::new(),
        };
        assert!(request.validate().is_err());
    }

    #[test]
    fn speech_absence_requires_complete_low_score_model_evidence() {
        let mut result = event_result(SpeechPresenceStatus::Absent, 0.02);
        result
            .validate()
            .expect("full low-score evidence is absent");

        result.coverage.status = AudioCoverageStatus::Partial;
        result.coverage.ratio = 0.75;
        result.coverage.analyzed_end_seconds = 1.5;
        result.coverage.analyzed_seconds = 1.5;
        assert!(result.validate().is_err());

        result.speech_presence.status = SpeechPresenceStatus::Unknown;
        result.validate().expect("partial coverage stays unknown");
    }

    #[test]
    fn speech_presence_states_follow_the_versioned_thresholds() {
        let mut result = event_result(SpeechPresenceStatus::Unknown, 0.12);
        result.validate().expect("between thresholds is unknown");

        result.speech_presence.status = SpeechPresenceStatus::Present;
        assert!(result.validate().is_err());
        result.speech_presence.max_score = 0.42;
        result.validate().expect("high score is present");

        result.speech_presence.status = SpeechPresenceStatus::Unknown;
        assert!(result.validate().is_err());
    }

    fn event_result(status: SpeechPresenceStatus, max_score: f64) -> EventDetectionResult {
        EventDetectionResult {
            object: "audio.event_detection".into(),
            events: vec![DetectedSoundEvent {
                class_id: "/m/015p6".into(),
                label: "Bird".into(),
                start_seconds: 0.0,
                end_seconds: 1.44,
                score: 0.82,
            }],
            speech_presence: SpeechPresence { status, max_score },
            coverage: AudioAnalysisCoverage {
                status: AudioCoverageStatus::Full,
                input_duration_seconds: 2.0,
                analyzed_start_seconds: 0.0,
                analyzed_end_seconds: 2.0,
                analyzed_seconds: 2.0,
                ratio: 1.0,
                window_count: 4,
                window_seconds: 0.96,
                hop_seconds: 0.48,
            },
            ontology: SoundEventOntology {
                id: "audioset".into(),
                revision: "yamnet-class-map@cdf24d193e19".into(),
                class_id_namespace: "audioset_mid".into(),
                class_count: 521,
                artifact_sha256: "cdf24d193e196d9e95912a2667051ae203e92a2ba09449218ccb40ef787c6df2"
                    .into(),
                license_spdx: "CC-BY-SA-4.0".into(),
            },
            policy: SoundEventDetectionPolicy {
                revision: "yamnet-audioset-event-policy-v1".into(),
                score_kind: "raw_sigmoid".into(),
                event_score_threshold: 0.1,
                smoothing: SoundEventSmoothingPolicy {
                    method: "centered_median_edge_padded".into(),
                    window_frames: 3,
                },
                max_classes_per_window: 12,
                max_events: 10_000,
                speech_class_set_revision: "yamnet-audioset-speech-family-indices-0-through-12-v1"
                    .into(),
                speech_present_threshold: 0.3,
                speech_absent_threshold: 0.05,
                max_audio_seconds: 600,
            },
            provenance: SoundEventProvenance {
                model: "google/yamnet/1".into(),
                model_archive_sha256:
                    "b80da2a1a56926fb0767205051a200dd7b3beaf3ea1ea126c42a53943996e5e0".into(),
                artifact_set_sha256:
                    "4730c9bde533285dc0b74b8b94c798273b40d91fc746b17f0c9281ac8764d6b8".into(),
                model_license_spdx: "Apache-2.0".into(),
                training_data_license_spdx: "CC-BY-4.0".into(),
                runtime: "tensorflow-saved-model".into(),
                runtime_version: "2.20.0".into(),
                decoder: "ffmpeg".into(),
                decoder_version: "ffmpeg 8.1.2".into(),
                preprocessing_identity:
                    "ffmpeg_decode_mono_f32le_16khz_then_tfhub_yamnet_waveform_v1".into(),
            },
        }
    }
}

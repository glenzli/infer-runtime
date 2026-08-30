//! Official Consumer client for Infer Runtime.
//!
//! The crate owns four cross-application concerns: Infra Discovery selection,
//! exact Core contract negotiation, bearer credential loading, and stable
//! error decoding. Capability-specific request and response types live in
//! additive modules rather than changing the Core contract.

mod audio;
mod contract;
mod discovery;
mod error;
mod jobs;
mod ocr;
mod raw_foundation;
mod responses;
mod retrieval;
mod transport;
mod vision;

pub use audio::{
    ALIGNMENT_CAPABILITIES, AUDIO_EMBEDDING_CAPABILITIES, AlignmentItem, AlignmentResponse,
    AudioAnalysisCoverage, AudioBytesResponse, AudioCoverageStatus, AudioEmbeddingProvenance,
    AudioEmbeddingResponse, AudioEmbeddingSpace, AudioEventDetectionResponse,
    AudioTextEmbeddingRequest, AudioTextQueryNormalizerProvenance, DetectedSoundEvent,
    EVENT_DETECTION_CAPABILITIES, ExecutionMode, SPEECH_CAPABILITIES, SoundEventDetectionPolicy,
    SoundEventOntology, SoundEventProvenance, SoundEventSmoothingPolicy, SpeechByteStream,
    SpeechFormat, SpeechPresence, SpeechPresenceStatus, SpeechRequest, TRANSCRIPTION_CAPABILITIES,
    TranscriptionFormat, TranscriptionLanguageEvidence, TranscriptionLanguageEvidenceSource,
    TranscriptionLanguageSegment, TranscriptionResponse,
};
pub use contract::{
    CAPABILITY_CATALOG_SCHEMA, CAPABILITY_CATALOG_VERSION, CAPABILITY_CONTRACT_HEADER,
    CONSUMER_CORE, CONSUMER_CORE_HEADER, CONSUMER_CORE_PROTOCOL, CONSUMER_CORE_VERSION,
    CONSUMER_OPENAPI_SHA256, CapabilityCatalog, CapabilityEntry, CapabilityRoute,
    CapabilitySchemaReference, ContractManifest, ContractRoute, Stability,
};
pub use discovery::{CONSUMER_HTTP_LOOPBACK_BINDING, DiscoveryResolver, ResolvedEndpoint};
pub use error::{Error, PublicError, PublicErrorEnvelope, Result};
pub use jobs::{
    AttemptSnapshot, CancelResult, CandidateDecision, ExplainResult, JobListItem, JobListPage,
    JobSnapshot, NamedRouteDecision, RoutingDecision,
};
pub use ocr::{DocumentOcrResponse, OcrProvenance, OcrTextLine};
pub use raw_foundation::{
    RAW_FOUNDATION_CAPABILITIES, RAW_FOUNDATION_ENDPOINT, RAW_FOUNDATION_INTENT,
    RAW_FOUNDATION_LEASE_ENDPOINT, RAW_FOUNDATION_STAGING_SCHEMA, RawFoundationArtifactReceipt,
    RawFoundationCancellation, RawFoundationExecuteResponse, RawFoundationLeaseBinding,
    RawFoundationLeaseGrant, RawFoundationLeaseRequest, RawFoundationPriority,
    RawFoundationProvenance, RawFoundationSource, RawFoundationStagingDescriptor,
};
pub use responses::{ResponsesEventStream, ResponsesRequest, ResponsesResult};
pub use retrieval::{
    RetrievalEmbeddingItem, RetrievalEmbeddingRequest, RetrievalEmbeddingResponse,
    RetrievalEmbeddingVector, RetrievalProvenance, RetrievalRerankRequest, RetrievalRerankResponse,
    RetrievalRerankResult, RetrievalTextInput,
};
pub use transport::{Client, ClientBuilder, CredentialSource};
pub use vision::{
    BoundingBox, ClassificationCategory, ClassificationDisposition, ClassificationReviewResponse,
    ClassificationSuggestion, EncodedCompletedRaster, EncodedLabelMap, EncodedSegmentationMask,
    EncodedSoftSegmentationMask, FaceDetection, FaceDetectionResponse, FaceEmbeddingEligibility,
    FaceEmbeddingResponse, FaceParsingOntology, FaceParsingRegion, FaceParsingResponse,
    FivePointLandmarks, IMAGE_COMPLETION_CAPABILITIES, ImageCompletionResponse,
    ImageDescriptionResponse, ImageDescriptionResult, ImageEmbeddingResponse, ImageGeometry,
    ImageUnderstandingProvenance, NormalizedBoundingBox, Point, SEMANTIC_GROUNDING_CAPABILITIES,
    SegmentationMaskRasterExtent, SegmentationPromptLabel, SegmentationPromptPoint,
    SemanticEmbeddingVector, SemanticGroundingRegion, SemanticGroundingResponse,
    SubjectSegmentationResponse, SubjectSegmentationSoftMaskResponse, TextEmbeddingRequest,
    TextEmbeddingResponse, VisionProvenance,
};

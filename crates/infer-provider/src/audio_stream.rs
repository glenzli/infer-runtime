//! Typed audio streaming provider contracts.

use std::{pin::Pin, sync::Arc};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::Stream;
use infer_core::{
    SpeechRequest, SpeechStreamDescriptor, TranscriptRevision, TranscriptionSessionRequest,
};

use crate::ProviderError;

pub type ProviderAudioByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>>;

pub struct SpeechStreamOutput {
    pub descriptor: SpeechStreamDescriptor,
    pub stream: ProviderAudioByteStream,
}

#[async_trait]
pub trait AudioStreamExecutor: Send + Sync {
    fn id(&self) -> &str;
    async fn execute_speech_stream(
        &self,
        physical_model: &str,
        request: SpeechRequest,
    ) -> Result<SpeechStreamOutput, ProviderError>;
}

pub type DynAudioStreamExecutor = Arc<dyn AudioStreamExecutor>;

#[async_trait]
pub trait AudioDuplexSession: Send {
    async fn push_audio(&mut self, chunk: Bytes) -> Result<(), ProviderError>;
    async fn commit(&mut self, is_final: bool) -> Result<TranscriptRevision, ProviderError>;
}

pub type DynAudioDuplexSession = Box<dyn AudioDuplexSession>;

#[async_trait]
pub trait AudioDuplexExecutor: Send + Sync {
    fn id(&self) -> &str;
    async fn open_transcription_session(
        &self,
        physical_model: &str,
        request: TranscriptionSessionRequest,
    ) -> Result<DynAudioDuplexSession, ProviderError>;
}

pub type DynAudioDuplexExecutor = Arc<dyn AudioDuplexExecutor>;

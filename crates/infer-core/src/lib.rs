//! Stable domain contracts for infer-runtime.
//!
//! The public surface is split by semantic ownership: Responses request intent
//! and constraints, static registry/configuration, and observable Job state.

mod audio;
mod audio_event;
mod config;
mod image_understanding;
mod job;
mod ocr;
mod payload;
mod request;
mod retrieval;
mod routing;
mod streaming;
mod vision;

use thiserror::Error;

pub use audio::*;
pub use audio_event::*;
pub use config::*;
pub use image_understanding::*;
pub use job::*;
pub use ocr::*;
pub use payload::*;
pub use request::*;
pub use retrieval::*;
pub use routing::*;
pub use streaming::*;
pub use vision::*;

pub const INFER_METADATA_PREFIX: &str = "infer.";
/// Infra Discovery protocol identifier for the dated Consumer Core.
pub const CONSUMER_CORE_PROTOCOL: &str = "infer-runtime.consumer-core";
/// Opaque protocol version published in the Infra Discovery offer.
pub const CONSUMER_CORE_VERSION: &str = "20260813.1";
/// The only complete Consumer Core identity accepted by this Runtime release.
pub const CONSUMER_CORE_CONTRACT: &str = "infer-runtime.consumer-core@20260813.1";
/// Required on every Consumer data/control request, including contract probe.
pub const CONSUMER_CORE_HEADER: &str = "Infer-Consumer-Contract";
/// Exact capability schema identity selected for one typed data-plane request.
pub const CAPABILITY_CONTRACT_HEADER: &str = "Infer-Capability-Contract";

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ContractError {
    #[error("`model` must name a configured intent profile")]
    MissingModel,
    #[error("unsupported Responses field: {0}")]
    UnsupportedField(&'static str),
    #[error("invalid infer metadata `{key}`: {message}")]
    InvalidMetadata { key: String, message: String },
    #[error("invalid audio request: {0}")]
    InvalidAudio(String),
    #[error("invalid vision request: {0}")]
    InvalidVision(String),
    #[error("configuration error: {0}")]
    Configuration(String),
}

macro_rules! string_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(
            Debug, Clone, Copy, serde::Deserialize, serde::Serialize,
            PartialEq, Eq, PartialOrd, Ord, Hash,
        )]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }

        impl std::str::FromStr for $name {
            type Err = ();
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value { $($value => Ok(Self::$variant),)+ _ => Err(()) }
            }
        }
    };
}

pub(crate) use string_enum;

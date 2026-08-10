//! Stable domain contracts for infer-runtime.
//!
//! The public surface is split by semantic ownership: Responses request intent
//! and constraints, static registry/configuration, and observable Job state.

mod audio;
mod config;
mod job;
mod payload;
mod request;
mod streaming;
mod vision;

use thiserror::Error;

pub use audio::*;
pub use config::*;
pub use job::*;
pub use payload::*;
pub use request::*;
pub use streaming::*;
pub use vision::*;

pub const INFER_METADATA_PREFIX: &str = "infer.";

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

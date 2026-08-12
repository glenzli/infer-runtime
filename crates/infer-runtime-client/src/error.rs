use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Infer Runtime discovery failed: {0}")]
    Discovery(String),
    #[error("Infer Runtime credential is unavailable: {0}")]
    Credential(String),
    #[error("Infer Runtime input is unavailable: {0}")]
    Input(String),
    #[error("Infer Runtime HTTP transport failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Infer Runtime Core contract is incompatible")]
    ContractMismatch,
    #[error("Infer Runtime returned HTTP {status} with code `{code}`: {message}")]
    Api {
        status: StatusCode,
        code: String,
        message: String,
    },
    #[error("Infer Runtime returned a malformed response: {0}")]
    MalformedResponse(String),
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PublicErrorEnvelope {
    pub error: PublicError,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PublicError {
    pub message: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub code: String,
}

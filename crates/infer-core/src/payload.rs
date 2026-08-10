//! Durable payload references shared by the encrypted spool and metadata store.

use serde::{Deserialize, Serialize};

use crate::string_enum;

string_enum!(DurablePayloadKind {
    ResponsesRequest => "responses_request",
    ResponsesResult => "responses_result"
});

/// The metadata database persists this reference, never the plaintext bytes or
/// encryption key. `digest` is a key-derived HMAC identity in addition to AEAD
/// authentication, so low-entropy prompts do not expose a guessable plain hash.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct DurablePayloadRef {
    pub blob_id: String,
    pub kind: DurablePayloadKind,
    pub digest: String,
    pub plaintext_bytes: usize,
}

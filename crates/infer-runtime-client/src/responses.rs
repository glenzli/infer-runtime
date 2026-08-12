use std::collections::BTreeMap;

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Client, Result, transport::ensure_success};

pub const RESPONSES_CAPABILITIES: &[&str] = &["infer.responses@20260812.1"];

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsesRequest {
    /// Stable Intent, never a Deployment or physical model name.
    pub model: String,
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Value>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResponsesResult {
    pub id: String,
    pub object: String,
    pub created_at: u64,
    pub model: String,
    pub status: String,
    #[serde(default)]
    pub output: Vec<Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

pub struct ResponsesEventStream {
    response: reqwest::Response,
    received: usize,
}

impl ResponsesEventStream {
    /// Returns the next raw SSE bytes. Frame parsing remains lossless: callers
    /// may buffer across chunks and apply the capability schema's SSE grammar.
    pub async fn next_chunk(&mut self) -> Result<Option<bytes::Bytes>> {
        let chunk = self.response.chunk().await?;
        if let Some(chunk) = &chunk {
            self.received = self.received.saturating_add(chunk.len());
            if self.received > 16 * 1024 * 1024 {
                return Err(crate::Error::MalformedResponse(
                    "Responses SSE stream exceeds 16777216 bytes".into(),
                ));
            }
        }
        Ok(chunk)
    }
}

impl Client {
    pub async fn create_response(&self, request: &ResponsesRequest) -> Result<ResponsesResult> {
        if request.stream {
            return Err(crate::Error::Input(
                "create_response is unary; the stable SDK does not yet expose Responses SSE".into(),
            ));
        }
        self.send_capability_json(
            RESPONSES_CAPABILITIES,
            Method::POST,
            "/v1/responses",
            Some(request),
        )
        .await
    }

    pub async fn stream_response(
        &self,
        request: &ResponsesRequest,
    ) -> Result<ResponsesEventStream> {
        if !request.stream || request.background {
            return Err(crate::Error::Input(
                "stream_response requires stream=true and background=false".into(),
            ));
        }
        let response = self
            .send_capability_with(RESPONSES_CAPABILITIES, |http, endpoint| {
                http.post(format!("{endpoint}/v1/responses")).json(request)
            })
            .await?;
        let response = ensure_success(response).await?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type.starts_with("text/event-stream") {
            return Err(crate::Error::MalformedResponse(
                "Responses stream omitted text/event-stream content type".into(),
            ));
        }
        Ok(ResponsesEventStream {
            response,
            received: 0,
        })
    }

    pub async fn get_response(&self, response_id: &str) -> Result<ResponsesResult> {
        validate_id(response_id)?;
        self.send_capability_json(
            RESPONSES_CAPABILITIES,
            Method::GET,
            &format!("/v1/responses/{response_id}"),
            Option::<&()>::None,
        )
        .await
    }

    pub async fn cancel_response(&self, response_id: &str) -> Result<crate::CancelResult> {
        validate_id(response_id)?;
        self.send_capability_json(
            RESPONSES_CAPABILITIES,
            Method::POST,
            &format!("/v1/responses/{response_id}/cancel"),
            Option::<&()>::None,
        )
        .await
    }
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id.contains('/') {
        Err(crate::Error::MalformedResponse(
            "invalid response id".into(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unary_responses_rejects_stream_before_discovery_or_transport() {
        let client = Client::builder().build().unwrap();
        let error = client
            .create_response(&ResponsesRequest {
                model: "assistant.general".into(),
                input: Value::String("bounded fixture".into()),
                instructions: None,
                stream: true,
                background: false,
                metadata: BTreeMap::new(),
                tools: Vec::new(),
                reasoning: None,
                max_output_tokens: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(error, crate::Error::Input(message) if message.contains("unary")));
    }
}

//! Authenticated control-plane client shared by local operator presentations.
//!
//! This owner knows inferd's observation and action endpoints. It deliberately
//! contains no terminal or web rendering and keeps the runtime credential on
//! the local process side of either presentation.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct EndpointSnapshot {
    pub(crate) value: Option<Value>,
    pub(crate) error: Option<String>,
}

impl EndpointSnapshot {
    fn from_result(result: Result<Value, String>) -> Self {
        match result {
            Ok(value) => Self {
                value: Some(value),
                error: None,
            },
            Err(error) => Self {
                value: None,
                error: Some(error),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ConsoleSnapshot {
    pub(crate) generation: u64,
    pub(crate) refreshed_at_unix_ms: u64,
    pub(crate) health: EndpointSnapshot,
    pub(crate) contract: EndpointSnapshot,
    pub(crate) metrics: EndpointSnapshot,
    pub(crate) jobs: EndpointSnapshot,
    pub(crate) providers: EndpointSnapshot,
    pub(crate) resources: EndpointSnapshot,
    pub(crate) budget: EndpointSnapshot,
}

impl Default for ConsoleSnapshot {
    fn default() -> Self {
        let pending = || EndpointSnapshot {
            value: None,
            error: Some("waiting for first refresh".into()),
        };
        Self {
            generation: 0,
            refreshed_at_unix_ms: 0,
            health: pending(),
            contract: pending(),
            metrics: pending(),
            jobs: pending(),
            providers: pending(),
            resources: pending(),
            budget: pending(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct OperatorClient {
    base_url: String,
    api_key: String,
    http: Client,
}

impl OperatorClient {
    pub(crate) fn new(base_url: String, api_key: String) -> anyhow::Result<Self> {
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            api_key,
            http: crate::consumer_http_client_builder()
                .timeout(Duration::from_secs(2))
                .build()
                .context("build operator HTTP client")?,
        })
    }

    pub(crate) async fn snapshot(&self, generation: u64) -> ConsoleSnapshot {
        let (health, contract, metrics, jobs, providers, resources, budget) = tokio::join!(
            self.get_json("/health", false),
            self.get_json("/infer/v1/contract", false),
            self.get_json("/infer/v1/metrics", true),
            self.get_json("/infer/v1/jobs?limit=50", true),
            self.get_json("/infer/v1/providers", true),
            self.get_json("/infer/v1/resources", true),
            self.get_json("/infer/v1/budget", true),
        );
        ConsoleSnapshot {
            generation,
            refreshed_at_unix_ms: unix_ms(),
            health: EndpointSnapshot::from_result(health),
            contract: EndpointSnapshot::from_result(contract),
            metrics: EndpointSnapshot::from_result(metrics),
            jobs: EndpointSnapshot::from_result(jobs),
            providers: EndpointSnapshot::from_result(providers),
            resources: EndpointSnapshot::from_result(resources),
            budget: EndpointSnapshot::from_result(budget),
        }
    }

    pub(crate) async fn health_reachable(&self) -> bool {
        self.get_json("/health", false).await.is_ok()
    }

    pub(crate) async fn cancel_job(&self, response_id: &str) -> Result<Value, String> {
        self.post_json(&format!("/infer/v1/jobs/{response_id}/cancel"), None)
            .await
    }

    pub(crate) async fn explain_job(&self, response_id: &str) -> Result<Value, String> {
        self.get_json(&format!("/infer/v1/explain/{response_id}"), true)
            .await
    }

    pub(crate) async fn refresh_resources(&self) -> Result<Value, String> {
        self.post_json("/infer/v1/resources", None).await
    }

    pub(crate) async fn load_resource(
        &self,
        provider: &str,
        deployment: &str,
    ) -> Result<Value, String> {
        self.post_json(
            &format!("/infer/v1/resources/{provider}/deployments/{deployment}/load"),
            None,
        )
        .await
    }

    pub(crate) async fn unload_resource(
        &self,
        provider: &str,
        deployment: &str,
    ) -> Result<Value, String> {
        self.post_json(
            &format!("/infer/v1/resources/{provider}/deployments/{deployment}/unload"),
            None,
        )
        .await
    }

    pub(crate) async fn probe_provider(&self, provider: &str) -> Result<Value, String> {
        self.post_json_with_timeout(
            &format!("/infer/v1/providers/{provider}/probe"),
            None,
            Duration::from_secs(5 * 60),
        )
        .await
    }

    pub(crate) async fn provider_models(&self, provider: &str) -> Result<Value, String> {
        self.get_json_with_timeout(
            &format!("/infer/v1/providers/{provider}/models"),
            true,
            Duration::from_secs(30),
        )
        .await
    }

    async fn get_json(&self, path: &str, authenticated: bool) -> Result<Value, String> {
        self.get_json_with_timeout(path, authenticated, Duration::from_secs(2))
            .await
    }

    async fn get_json_with_timeout(
        &self,
        path: &str,
        authenticated: bool,
        request_timeout: Duration,
    ) -> Result<Value, String> {
        let mut request = self.http.get(format!("{}{}", self.base_url, path));
        if authenticated {
            request = request.bearer_auth(&self.api_key);
        }
        let response = request
            .timeout(request_timeout)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        decode_json(response).await
    }

    async fn post_json(&self, path: &str, body: Option<&Value>) -> Result<Value, String> {
        self.post_json_with_timeout(path, body, Duration::from_secs(2))
            .await
    }

    async fn post_json_with_timeout(
        &self,
        path: &str,
        body: Option<&Value>,
        request_timeout: Duration,
    ) -> Result<Value, String> {
        let mut request = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .bearer_auth(&self.api_key)
            .timeout(request_timeout);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        decode_json(response).await
    }
}

async fn decode_json(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    let value = response
        .json::<Value>()
        .await
        .map_err(|error| format!("{status}: invalid JSON response: {error}"))?;
    if status.is_success() {
        Ok(value)
    } else {
        let message = value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("request failed");
        let code = value
            .pointer("/error/code")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error");
        Err(format!("{status} {code}: {message}"))
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

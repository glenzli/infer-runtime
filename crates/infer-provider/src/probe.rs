//! Explicit, billable provider contract probes for the Responses protocol.

use infer_core::{
    ProviderCapability, ProviderCapabilityProfile, ReasoningConfig, ReasoningEffort,
    ResponsesRequest, ToolChoice,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{Provider, ProviderError, ProviderFailureKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProbeStatus {
    Passed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderProbeCheck {
    pub capability: ProviderCapability,
    pub status: ProviderProbeStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ProviderFailureKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderProbeReport {
    pub provider: String,
    pub model: String,
    pub profile_version: u16,
    pub declared_capabilities: Vec<ProviderCapability>,
    pub passed: bool,
    pub checks: Vec<ProviderProbeCheck>,
}

/// Runs the concrete checks declared by one Responses capability profile.
///
/// A probe intentionally never runs automatically: it sends real provider
/// requests and may consume a small amount of quota. The first failed check
/// stops the remaining checks so a broken endpoint is not needlessly loaded.
pub async fn probe_responses_provider(
    provider: &dyn Provider,
    model: &str,
    profile: &ProviderCapabilityProfile,
) -> ProviderProbeReport {
    probe_responses_provider_with_effort(provider, model, profile, None).await
}

/// Variant used by bridges whose admitted Deployment does not support the
/// Responses sentinel effort `none`.
pub async fn probe_responses_provider_with_effort(
    provider: &dyn Provider,
    model: &str,
    profile: &ProviderCapabilityProfile,
    reasoning_effort: Option<ReasoningEffort>,
) -> ProviderProbeReport {
    let declared_capabilities: Vec<_> = profile.capabilities.iter().copied().collect();
    let mut checks = Vec::with_capacity(declared_capabilities.len());
    let mut prior_failure = false;
    for capability in &declared_capabilities {
        if prior_failure {
            checks.push(ProviderProbeCheck {
                capability: *capability,
                status: ProviderProbeStatus::Skipped,
                error_kind: None,
                error: None,
            });
            continue;
        }
        match probe_capability(provider, model, *capability, reasoning_effort).await {
            Ok(()) => checks.push(ProviderProbeCheck {
                capability: *capability,
                status: ProviderProbeStatus::Passed,
                error_kind: None,
                error: None,
            }),
            Err(error) => {
                prior_failure = true;
                checks.push(ProviderProbeCheck {
                    capability: *capability,
                    status: ProviderProbeStatus::Failed,
                    error_kind: Some(error.kind()),
                    error: Some(error.to_string()),
                });
            }
        }
    }
    ProviderProbeReport {
        provider: provider.id().into(),
        model: model.into(),
        profile_version: profile.version,
        declared_capabilities,
        passed: !prior_failure,
        checks,
    }
}

async fn probe_capability(
    provider: &dyn Provider,
    model: &str,
    capability: ProviderCapability,
    reasoning_effort: Option<ReasoningEffort>,
) -> Result<(), ProviderError> {
    let mut request = base_request(model);
    match capability {
        ProviderCapability::Responses => {
            provider.execute(request).await?;
        }
        ProviderCapability::Instructions => {
            request.instructions = Some(Value::String("Reply only with ok.".into()));
            provider.execute(request).await?;
        }
        ProviderCapability::Streaming => {
            request.stream = true;
            let mut stream = provider.execute_stream(request).await?;
            match futures_util::StreamExt::next(&mut stream).await {
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error),
                None => {
                    return Err(ProviderError::Protocol(
                        "probe stream ended before its first event".into(),
                    ));
                }
            }
        }
        ProviderCapability::FunctionTools => {
            request.tools = vec![json!({
                "type": "function",
                "name": "probe",
                "description": "A provider compatibility probe; do not call it.",
                "parameters": {"type": "object", "properties": {}}
            })];
            provider.execute(request).await?;
        }
        ProviderCapability::WebSearch => {
            request.input = Value::String(
                "Use web search to identify the official domain example.com and answer only ok."
                    .into(),
            );
            request.tools = vec![json!({
                "type": "web_search",
                "external_web_access": false
            })];
            request.tool_choice = Some(ToolChoice::Required);
            let response = provider.execute(request).await?;
            if response
                .get("output")
                .and_then(Value::as_array)
                .is_none_or(|items| {
                    !items.iter().any(|item| {
                        item.get("type").and_then(Value::as_str) == Some("web_search_call")
                    })
                })
            {
                return Err(ProviderError::Protocol(
                    "web search probe did not return a web_search_call item".into(),
                ));
            }
        }
        ProviderCapability::ImageGeneration => {
            request.input = Value::String(
                "Generate one simple solid blue circle on a white background, with no text.".into(),
            );
            request.tools = vec![json!({"type": "image_generation"})];
            let response = provider.execute(request).await?;
            if response.pointer("/output/0/type").and_then(Value::as_str)
                != Some("image_generation_call")
                || response
                    .pointer("/output/0/result")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            {
                return Err(ProviderError::Protocol(
                    "image generation probe did not return one encoded image".into(),
                ));
            }
        }
        ProviderCapability::AgentTask => {
            return Err(ProviderError::InvalidInput(
                "Agent task probe requires verified per-task input isolation; capability is not active".into(),
            ));
        }
        ProviderCapability::ReasoningEffort => {
            request.reasoning = Some(ReasoningConfig {
                // Endpoint capability and deployment effort range are
                // separate contracts. `none` proves the standard field is
                // honored without assuming this particular model supports a
                // higher reasoning tier.
                effort: Some(reasoning_effort.unwrap_or(ReasoningEffort::None)),
                extra: Default::default(),
            });
            provider.execute(request).await?;
        }
        ProviderCapability::Temperature => {
            request.temperature = Some(0.2);
            provider.execute(request).await?;
        }
        ProviderCapability::TopP => {
            request.top_p = Some(0.9);
            provider.execute(request).await?;
        }
        ProviderCapability::MaxOutputTokens => {
            request.max_output_tokens = Some(16);
            provider.execute(request).await?;
        }
        ProviderCapability::Truncation => {
            request.truncation = Some("auto".into());
            provider.execute(request).await?;
        }
        ProviderCapability::Metadata => {
            request.metadata.insert("probe".into(), "true".into());
            provider.execute(request).await?;
        }
    }
    Ok(())
}

fn base_request(model: &str) -> ResponsesRequest {
    ResponsesRequest {
        model: model.into(),
        input: Value::String("Reply only with ok.".into()),
        instructions: None,
        stream: false,
        background: false,
        metadata: Default::default(),
        tools: vec![],
        tool_choice: None,
        reasoning: None,
        temperature: None,
        top_p: None,
        max_output_tokens: None,
        truncation: None,
        store: Some(false),
        previous_response_id: None,
        conversation: None,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, sync::Arc};

    use async_trait::async_trait;
    use bytes::Bytes;
    use futures_util::stream;
    use tokio::sync::Mutex;

    use super::*;
    use crate::ProviderByteStream;
    use infer_core::ProviderProtocol;

    struct RecordingProvider {
        requests: Mutex<Vec<ResponsesRequest>>,
        fail_temperature: bool,
    }

    #[async_trait]
    impl Provider for RecordingProvider {
        fn id(&self) -> &str {
            "recording"
        }

        async fn execute(&self, request: ResponsesRequest) -> Result<Value, ProviderError> {
            if self.fail_temperature && request.temperature.is_some() {
                return Err(ProviderError::Upstream {
                    status: 400,
                    body: "temperature unsupported".into(),
                    retry_after: None,
                });
            }
            let image_generation = request.requests_image_generation();
            let web_search = request.requests_web_search();
            self.requests.lock().await.push(request);
            if image_generation {
                Ok(json!({
                    "id": "probe",
                    "output": [{
                        "type": "image_generation_call",
                        "result": "AA=="
                    }]
                }))
            } else if web_search {
                Ok(json!({
                    "id": "probe",
                    "output": [{
                        "type": "web_search_call",
                        "status": "completed",
                        "action": {"type": "search", "query": "example.com"}
                    }]
                }))
            } else {
                Ok(json!({"id":"probe"}))
            }
        }

        async fn execute_stream(
            &self,
            request: ResponsesRequest,
        ) -> Result<ProviderByteStream, ProviderError> {
            self.requests.lock().await.push(request);
            Ok(Box::pin(stream::iter(vec![Ok(Bytes::from(
                "event: probe\n\n",
            ))])))
        }
    }

    fn profile(capabilities: BTreeSet<ProviderCapability>) -> ProviderCapabilityProfile {
        ProviderCapabilityProfile {
            version: 1,
            protocol: ProviderProtocol::Responses,
            capabilities,
        }
    }

    #[tokio::test]
    async fn probes_every_declared_responses_behavior() {
        let provider = Arc::new(RecordingProvider {
            requests: Mutex::new(vec![]),
            fail_temperature: false,
        });
        let report = probe_responses_provider(
            provider.as_ref(),
            "probe-model",
            &profile(BTreeSet::from([
                ProviderCapability::Responses,
                ProviderCapability::Instructions,
                ProviderCapability::Streaming,
                ProviderCapability::FunctionTools,
                ProviderCapability::WebSearch,
                ProviderCapability::ImageGeneration,
                ProviderCapability::ReasoningEffort,
                ProviderCapability::Temperature,
                ProviderCapability::TopP,
                ProviderCapability::MaxOutputTokens,
                ProviderCapability::Truncation,
                ProviderCapability::Metadata,
            ])),
        )
        .await;
        assert!(report.passed);
        assert!(
            report
                .checks
                .iter()
                .all(|check| check.status == ProviderProbeStatus::Passed)
        );
        let requests = provider.requests.lock().await;
        assert!(requests.iter().any(|request| request.stream));
        assert!(
            requests
                .iter()
                .any(|request| request.instructions.is_some())
        );
        assert!(requests.iter().any(|request| !request.tools.is_empty()));
        assert!(
            requests
                .iter()
                .any(ResponsesRequest::requests_image_generation)
        );
        assert!(requests.iter().any(ResponsesRequest::requests_web_search));
        assert!(
            requests
                .iter()
                .any(|request| { request.reasoning_effort() == Some(ReasoningEffort::None) })
        );
        assert!(requests.iter().any(|request| request.temperature.is_some()));
        assert!(requests.iter().any(|request| request.top_p.is_some()));
        assert!(
            requests
                .iter()
                .any(|request| request.max_output_tokens.is_some())
        );
        assert!(requests.iter().any(|request| request.truncation.is_some()));
        assert!(requests.iter().any(|request| !request.metadata.is_empty()));
    }

    #[tokio::test]
    async fn failed_probe_stops_the_remaining_checks() {
        let provider = RecordingProvider {
            requests: Mutex::new(vec![]),
            fail_temperature: true,
        };
        let report = probe_responses_provider(
            &provider,
            "probe-model",
            &profile(BTreeSet::from([
                ProviderCapability::Responses,
                ProviderCapability::Temperature,
                ProviderCapability::TopP,
            ])),
        )
        .await;
        assert!(!report.passed);
        assert_eq!(report.checks[1].status, ProviderProbeStatus::Failed);
        assert_eq!(
            report.checks[1].error_kind,
            Some(ProviderFailureKind::InvalidRequest)
        );
        assert_eq!(report.checks[2].status, ProviderProbeStatus::Skipped);
    }

    #[tokio::test]
    async fn agent_task_declaration_cannot_pass_a_responses_probe() {
        let provider = RecordingProvider {
            requests: Mutex::new(vec![]),
            fail_temperature: false,
        };
        let report = probe_responses_provider(
            &provider,
            "probe-model",
            &profile(BTreeSet::from([ProviderCapability::AgentTask])),
        )
        .await;
        assert!(!report.passed);
        assert_eq!(report.checks[0].status, ProviderProbeStatus::Failed);
        assert!(provider.requests.lock().await.is_empty());
    }
}

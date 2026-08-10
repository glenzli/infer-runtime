//! Restricted Antigravity CLI bridge for subscription-backed inference.
//!
//! The adapter treats the current user's signed-in CLI as a trusted local
//! subscription agent. Authentication remains owned by the real user session;
//! Consumer payloads and explicit diagnostics use an ephemeral project, agent
//! capabilities are not exposed, and only inference-shaped events are accepted.

mod execution;
mod process;

use std::{collections::BTreeSet, time::Duration};

use async_trait::async_trait;
use infer_core::ResponsesRequest;
use serde_json::Value;

use crate::{
    Provider, ProviderByteStream, ProviderError, ProviderFailureKind, ProviderModelCatalog,
    ProviderModelInfo,
};
use execution::{SessionExecution, validate_effort};
use process::{classify_failed_process, reported_process_failure, run};

const MAX_CATALOG_BYTES: usize = 256 * 1024;

pub struct AntigravityCliProvider {
    id: String,
    command: String,
    args: Vec<String>,
    admitted_models: BTreeSet<String>,
}

impl AntigravityCliProvider {
    pub fn new(
        id: impl Into<String>,
        command: impl Into<String>,
        args: Vec<String>,
        admitted_models: BTreeSet<String>,
    ) -> Self {
        Self {
            id: id.into(),
            command: command.into(),
            args,
            admitted_models,
        }
    }

    pub async fn discover_model_catalog(&self) -> Result<ProviderModelCatalog, ProviderError> {
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            run(
                &self.command,
                &self.args,
                &["models".into()],
                MAX_CATALOG_BYTES,
            ),
        )
        .await
        .map_err(|_| ProviderError::Classified {
            kind: ProviderFailureKind::Timeout,
            message: "Antigravity model inventory timed out".into(),
        })??;
        if !output.status.success() {
            return Err(classify_failed_process(&output));
        }
        if let Some(error) = reported_process_failure(&output) {
            return Err(error);
        }
        let stdout = std::str::from_utf8(&output.stdout)
            .map_err(|_| ProviderError::Protocol("Antigravity model list was not UTF-8".into()))?;
        let models = parse_model_catalog(stdout, &self.admitted_models)?;
        Ok(ProviderModelCatalog {
            provider: self.id.clone(),
            models,
        })
    }

    async fn execute_inner(&self, request: ResponsesRequest) -> Result<Value, ProviderError> {
        validate_request(&request)?;
        if !self.admitted_models.contains(&request.model) {
            return Err(ProviderError::InvalidInput(
                "Antigravity model was discovered but is not admitted by a Deployment".into(),
            ));
        }
        validate_effort(
            &request.model,
            request
                .reasoning
                .as_ref()
                .and_then(|reasoning| reasoning.effort),
        )?;
        let execution = SessionExecution::prepare(&request)?;
        execution
            .execute(&self.command, &self.args, &request.model)
            .await
    }
}

#[async_trait]
impl Provider for AntigravityCliProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn execute(&self, request: ResponsesRequest) -> Result<Value, ProviderError> {
        self.execute_inner(request).await
    }

    async fn execute_stream(
        &self,
        _request: ResponsesRequest,
    ) -> Result<ProviderByteStream, ProviderError> {
        Err(ProviderError::InvalidInput(
            "Antigravity bridge currently supports unary execution only".into(),
        ))
    }

    async fn model_catalog(&self) -> Result<Option<ProviderModelCatalog>, ProviderError> {
        self.discover_model_catalog().await.map(Some)
    }
}

fn validate_request(request: &ResponsesRequest) -> Result<(), ProviderError> {
    if request.stream {
        return Err(ProviderError::InvalidInput(
            "Antigravity bridge currently supports unary execution only".into(),
        ));
    }
    if !request.tools.is_empty() {
        return Err(ProviderError::InvalidInput(
            "tools are not exposed by the Antigravity bridge".into(),
        ));
    }
    if request.temperature.is_some()
        || request.top_p.is_some()
        || request.max_output_tokens.is_some()
        || request.truncation.is_some()
        || !request.metadata.is_empty()
    {
        return Err(ProviderError::InvalidInput(
            "request uses a field outside the Antigravity bridge subset".into(),
        ));
    }
    if request
        .reasoning
        .as_ref()
        .is_some_and(|reasoning| !reasoning.extra.is_empty())
    {
        return Err(ProviderError::InvalidInput(
            "Antigravity bridge supports reasoning.effort only".into(),
        ));
    }
    if request.background
        || request.store == Some(true)
        || request.previous_response_id.is_some()
        || request.conversation.is_some()
    {
        return Err(ProviderError::InvalidInput(
            "Antigravity bridge is stateless and does not expose durable or conversation state"
                .into(),
        ));
    }
    Ok(())
}

fn parse_model_catalog(
    output: &str,
    admitted_models: &BTreeSet<String>,
) -> Result<Vec<ProviderModelInfo>, ProviderError> {
    let mut models = Vec::new();
    let mut seen = BTreeSet::new();
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let lowered = line.to_ascii_lowercase();
        if lowered.starts_with("fetching ")
            || lowered.starts_with("available models")
            || lowered.starts_with("model\t")
        {
            continue;
        }
        let columns = line.split('\t').map(str::trim).collect::<Vec<_>>();
        let model = columns[0]
            .trim_start_matches(['*', '>', '✓'])
            .trim()
            .to_owned();
        if model.eq_ignore_ascii_case("model") || !valid_model_slug(&model) {
            return Err(ProviderError::Protocol(
                "Antigravity model inventory contained an invalid model slug".into(),
            ));
        }
        if !seen.insert(model.clone()) {
            continue;
        }
        let display_name = columns
            .get(1)
            .filter(|value| !value.is_empty())
            .copied()
            .unwrap_or(&model)
            .to_owned();
        let supported_reasoning_efforts = model_efforts(&model);
        let default_reasoning_effort = model_default_effort(&model).into();
        models.push(ProviderModelInfo {
            id: model.clone(),
            admitted: admitted_models.contains(&model),
            model,
            display_name,
            description: "Antigravity CLI subscription model".into(),
            input_modalities: vec!["text".into()],
            supported_reasoning_efforts,
            default_reasoning_effort,
            is_default: false,
            hidden: false,
            upgrade: None,
        });
    }
    if models.is_empty() {
        return Err(ProviderError::Classified {
            kind: ProviderFailureKind::Unavailable,
            message: "Antigravity model inventory was empty".into(),
        });
    }
    Ok(models)
}

fn model_efforts(model: &str) -> Vec<String> {
    if model.ends_with("-low") {
        vec!["low".into()]
    } else if model.ends_with("-medium") {
        vec!["none".into(), "medium".into()]
    } else if model.ends_with("-high") {
        vec!["high".into()]
    } else {
        Vec::new()
    }
}

fn model_default_effort(model: &str) -> &'static str {
    if model.ends_with("-low") {
        "low"
    } else if model.ends_with("-medium") {
        "medium"
    } else if model.ends_with("-high") {
        "high"
    } else {
        ""
    }
}

fn valid_model_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'/')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dynamic_group_without_auto_admitting_models() {
        let models = parse_model_catalog(
            "Fetching available models...\nflash\tFlash\npro\tPro\n",
            &BTreeSet::from(["flash".into()]),
        )
        .unwrap();
        assert_eq!(models.len(), 2);
        assert!(models[0].admitted);
        assert!(!models[1].admitted);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_cli_proves_catalog_without_auto_admission() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("fake-agy");
        std::fs::write(
            &script,
            r##"#!/bin/sh
case " $* " in
*" models "*)
  printf 'gemini-3.6-flash-medium\tGemini Flash Medium\npro\tPro\n'
  exit 0
  ;;
*" --output-format stream-json "*)
  model='gemini-3.6-flash-medium'
  printf '{"event":"init","conversation_id":"fake-conversation","init":{"model":"%s","cwd":"%s"}}\n' "$model" "$PWD"
  printf '{"event":"step_update","step_update":{"conversation_id":"fake-conversation","step_index":0,"state":"DONE","step_type":"agent_response","usage":{"input_tokens":3,"output_tokens":1,"thinking_tokens":0,"cache_read_tokens":0,"total_tokens":4}}}\n'
  printf '{"event":"result","result":{"conversation_id":"fake-conversation","status":"SUCCESS","response":"ok","usage":{"input_tokens":3,"output_tokens":1,"thinking_tokens":0,"cache_read_tokens":0,"total_tokens":4}}}\n'
  exit 0
  ;;
esac
"##,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script, permissions).unwrap();
        let provider = AntigravityCliProvider::new(
            "agy-test",
            script.display().to_string(),
            vec![],
            BTreeSet::from(["gemini-3.6-flash-medium".into()]),
        );
        let catalog = provider.discover_model_catalog().await.unwrap();
        assert_eq!(catalog.models.len(), 2);
        assert_eq!(
            catalog.models.iter().filter(|model| model.admitted).count(),
            1
        );
        // Request parsing and wire behavior are covered in `execution`; a
        // signed-in real smoke covers the user-session boundary.
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "requires the current user's signed-in Antigravity CLI"]
    async fn real_signed_in_cli_completes_in_the_user_session() {
        let command = "/opt/homebrew/bin/agy";
        assert!(std::path::Path::new(command).is_file());
        let model = "gemini-3.6-flash-medium";
        let provider = AntigravityCliProvider::new(
            "antigravity-real",
            command,
            vec![],
            BTreeSet::from([model.into()]),
        );
        let response = provider
            .execute(ResponsesRequest {
                model: model.into(),
                input: serde_json::json!("Reply with exactly: bridge-ok"),
                instructions: Some(serde_json::json!(
                    "Return the requested literal and nothing else."
                )),
                stream: false,
                background: false,
                metadata: Default::default(),
                tools: vec![],
                reasoning: None,
                temperature: None,
                top_p: None,
                max_output_tokens: None,
                truncation: None,
                store: Some(false),
                previous_response_id: None,
                conversation: None,
            })
            .await
            .expect("signed-in Antigravity inference must succeed");
        assert_eq!(response["model"], model);
        assert_eq!(response["status"], "completed");
        assert_eq!(response["output"][0]["content"][0]["text"], "bridge-ok");
    }
}

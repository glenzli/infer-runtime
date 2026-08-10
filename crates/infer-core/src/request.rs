//! OpenAI Responses-compatible request fields and runtime-only constraints.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ContractError, ExecutionMode, INFER_METADATA_PREFIX, IntentProfile, ProviderAccessClass,
    string_enum,
};

/// The explicitly supported, stateless subset of an OpenAI Responses request.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsesRequest {
    /// A stable intent profile such as `text.summarize`, never a deployment ID.
    pub model: String,
    pub input: Value,
    #[serde(default)]
    pub instructions: Option<Value>,
    #[serde(default)]
    pub stream: bool,
    /// Standard Responses background mode. infer-runtime currently accepts it
    /// only for non-streaming, local-only durable execution.
    #[serde(default, skip_serializing_if = "is_false")]
    pub background: bool,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub truncation: Option<String>,
    #[serde(default)]
    pub store: Option<bool>,
    #[serde(default)]
    pub previous_response_id: Option<String>,
    #[serde(default)]
    pub conversation: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ReasoningConfig {
    #[serde(default)]
    pub effort: Option<ReasoningEffort>,
    /// Preserve provider-compatible fields such as mode/context even while the
    /// runtime only reasons about `effort` for candidate eligibility.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl ResponsesRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.model.trim().is_empty() {
            return Err(ContractError::MissingModel);
        }
        if self.previous_response_id.is_some() {
            return Err(ContractError::UnsupportedField("previous_response_id"));
        }
        if self.conversation.is_some() {
            return Err(ContractError::UnsupportedField("conversation"));
        }
        if self.store == Some(true) {
            return Err(ContractError::UnsupportedField("store=true"));
        }
        if self.background && self.stream {
            return Err(ContractError::UnsupportedField(
                "stream=true with background=true",
            ));
        }
        RequestConstraints::from_metadata(&self.metadata).map(|_| ())
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        RequestConstraints::from_metadata(&self.metadata)
    }

    pub fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.effort)
    }

    /// Merge task-level generation defaults into an otherwise valid public
    /// request. A caller's explicit values are never tightened or replaced.
    pub fn apply_intent_defaults(&mut self, intent: &IntentProfile) {
        if self.max_output_tokens.is_none() {
            self.max_output_tokens = intent.default_max_output_tokens;
        }
        if self.reasoning.is_none()
            && let Some(effort) = intent.default_reasoning_effort
        {
            self.reasoning = Some(ReasoningConfig {
                effort: Some(effort),
                extra: BTreeMap::new(),
            });
        }
    }

    /// The endpoint and model behaviors required by the public request. These
    /// are checked during candidate planning, before an upstream request is
    /// attempted.
    pub fn execution_requirements(&self) -> ExecutionRequirements {
        let mut requirements = ExecutionRequirements::responses();
        requirements.execution_mode = if self.stream {
            ExecutionMode::ServerStream
        } else {
            ExecutionMode::Unary
        };
        requirements.input_modalities = response_input_modalities(&self.input);
        if self.instructions.is_some() {
            requirements
                .provider_capabilities
                .insert(ProviderCapability::Instructions);
        }
        if self.stream {
            requirements
                .provider_capabilities
                .insert(ProviderCapability::Streaming);
        }
        if !self.tools.is_empty() {
            requirements
                .provider_capabilities
                .insert(ProviderCapability::FunctionTools);
            requirements.model_features.insert("function_tools".into());
        }
        if self.reasoning.is_some() {
            requirements
                .provider_capabilities
                .insert(ProviderCapability::ReasoningEffort);
            if self
                .reasoning_effort()
                .is_some_and(|effort| effort != ReasoningEffort::None)
            {
                requirements.model_features.insert("reasoning".into());
            }
        }
        if self.temperature.is_some() {
            requirements
                .provider_capabilities
                .insert(ProviderCapability::Temperature);
        }
        if self.top_p.is_some() {
            requirements
                .provider_capabilities
                .insert(ProviderCapability::TopP);
        }
        if self.max_output_tokens.is_some() {
            requirements
                .provider_capabilities
                .insert(ProviderCapability::MaxOutputTokens);
        }
        if self.truncation.is_some() {
            requirements
                .provider_capabilities
                .insert(ProviderCapability::Truncation);
        }
        if self
            .metadata
            .keys()
            .any(|key| !key.starts_with(INFER_METADATA_PREFIX))
        {
            requirements
                .provider_capabilities
                .insert(ProviderCapability::Metadata);
        }
        requirements
    }

    /// Returns a request suitable for an upstream provider. Runtime-only
    /// metadata never crosses this boundary.
    pub fn for_provider(&self, physical_model: String) -> Self {
        let mut request = self.clone();
        request.model = physical_model;
        request
            .metadata
            .retain(|key, _| !key.starts_with(INFER_METADATA_PREFIX));
        request.store = Some(false);
        request.background = false;
        request
    }
}

/// Input-dependent behavior required of a concrete candidate in addition to an
/// Intent's static modality and feature contract.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutionRequirements {
    pub provider_capabilities: BTreeSet<ProviderCapability>,
    pub model_features: BTreeSet<String>,
    pub input_modalities: BTreeSet<Modality>,
    pub execution_mode: ExecutionMode,
}

impl ExecutionRequirements {
    pub fn responses() -> Self {
        Self {
            provider_capabilities: BTreeSet::from([ProviderCapability::Responses]),
            model_features: BTreeSet::new(),
            input_modalities: BTreeSet::from([Modality::Text]),
            execution_mode: ExecutionMode::Unary,
        }
    }
}

/// Classifies only public input payloads that have a stable modality meaning.
/// Unknown extension objects remain text-compatible, but image/audio/video
/// markers are detected recursively so cloud egress policy fails closed.
fn response_input_modalities(value: &Value) -> BTreeSet<Modality> {
    fn visit(value: &Value, modalities: &mut BTreeSet<Modality>) {
        match value {
            Value::String(_) => {
                modalities.insert(Modality::Text);
            }
            Value::Array(values) => {
                for value in values {
                    visit(value, modalities);
                }
            }
            Value::Object(object) => {
                match object.get("type").and_then(Value::as_str) {
                    Some("input_image" | "image" | "image_url" | "local_image") => {
                        modalities.insert(Modality::Image);
                    }
                    Some("input_audio" | "audio") => {
                        modalities.insert(Modality::Audio);
                    }
                    Some("input_video" | "video") => {
                        modalities.insert(Modality::Video);
                    }
                    Some("input_text" | "text" | "message") => {
                        modalities.insert(Modality::Text);
                    }
                    _ => {}
                }
                if object.contains_key("image_url") {
                    modalities.insert(Modality::Image);
                }
                if object.contains_key("audio_url") || object.contains_key("input_audio") {
                    modalities.insert(Modality::Audio);
                }
                for (key, value) in object {
                    if matches!(key.as_str(), "content" | "input") {
                        visit(value, modalities);
                    }
                    if key == "text" && value.is_string() {
                        modalities.insert(Modality::Text);
                    }
                }
            }
            _ => {}
        }
    }

    let mut modalities = BTreeSet::new();
    visit(value, &mut modalities);
    if modalities.is_empty() {
        modalities.insert(Modality::Text);
    }
    modalities
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct RequestConstraints {
    pub policy: Option<String>,
    pub priority: Option<Priority>,
    /// Optional hard narrowing of the Provider access boundary. This never
    /// grants access: the selected class must still be present in the App ACL.
    pub provider_access_class: Option<ProviderAccessClass>,
    pub placement: Option<PlacementScope>,
    pub prefer: Option<PlacementPreference>,
    pub offline_required: Option<bool>,
    pub quality_floor: Option<QualityGrade>,
    pub latency: Option<Latency>,
    pub max_cost_usd: Option<f64>,
    pub fallback: Option<Fallback>,
    pub deadline_ms: Option<u64>,
}

impl RequestConstraints {
    pub fn from_metadata(metadata: &BTreeMap<String, String>) -> Result<Self, ContractError> {
        let mut result = Self::default();
        for (key, value) in metadata
            .iter()
            .filter(|(key, _)| key.starts_with(INFER_METADATA_PREFIX))
        {
            let invalid = |message: &str| ContractError::InvalidMetadata {
                key: key.clone(),
                message: message.to_owned(),
            };
            match key.as_str() {
                "infer.policy" => result.policy = Some(value.clone()),
                "infer.priority" => {
                    result.priority = Some(
                        value
                            .parse()
                            .map_err(|_| invalid("expected interactive, normal, or background"))?,
                    )
                }
                "infer.provider_access_class" => {
                    result.provider_access_class = Some(
                        value
                            .parse()
                            .map_err(|_| invalid("expected standard or subscription"))?,
                    )
                }
                "infer.placement" => {
                    result.placement = Some(value.parse().map_err(|_| {
                        invalid("expected local_only, private, anywhere, or cloud_only")
                    })?)
                }
                "infer.prefer" => {
                    result.prefer = Some(
                        value
                            .parse()
                            .map_err(|_| invalid("expected local, trusted_node, or cloud"))?,
                    )
                }
                "infer.offline_required" => {
                    result.offline_required = Some(
                        value
                            .parse()
                            .map_err(|_| invalid("expected true or false"))?,
                    )
                }
                "infer.quality_floor" => {
                    result.quality_floor =
                        Some(value.parse().map_err(|_| {
                            invalid("expected basic, general, advanced, or frontier")
                        })?)
                }
                "infer.latency" => {
                    result.latency =
                        Some(value.parse().map_err(|_| {
                            invalid("expected interactive, balanced, or throughput")
                        })?)
                }
                "infer.max_cost_usd" => {
                    let amount: f64 = value
                        .parse()
                        .map_err(|_| invalid("expected a decimal string"))?;
                    if !amount.is_finite() || amount < 0.0 {
                        return Err(invalid("must be a finite non-negative decimal"));
                    }
                    result.max_cost_usd = Some(amount);
                }
                "infer.fallback" => {
                    result.fallback = Some(value.parse().map_err(|_| {
                        invalid("expected none, equivalent, or allow_lower_quality")
                    })?)
                }
                "infer.deadline_ms" => {
                    let deadline: u64 = value
                        .parse()
                        .map_err(|_| invalid("expected a positive integer"))?;
                    if deadline == 0 {
                        return Err(invalid("must be positive"));
                    }
                    result.deadline_ms = Some(deadline);
                }
                _ => return Err(invalid("unknown reserved infer.* key")),
            }
        }
        Ok(result)
    }
}

string_enum!(Priority { Interactive => "interactive", Normal => "normal", Background => "background" });
string_enum!(Placement { Local => "local", TrustedNode => "trusted_node", Cloud => "cloud" });
string_enum!(PlacementScope { LocalOnly => "local_only", Private => "private", Anywhere => "anywhere", CloudOnly => "cloud_only" });
string_enum!(PlacementPreference { Local => "local", TrustedNode => "trusted_node", Cloud => "cloud" });
string_enum!(QualityGrade { Basic => "basic", General => "general", Advanced => "advanced", Frontier => "frontier" });
string_enum!(RatingStatus { Provisional => "provisional", Benchmarked => "benchmarked" });
string_enum!(ResourceClass { Light => "light", Standard => "standard", Heavy => "heavy", Extreme => "extreme" });
string_enum!(ReasoningEffort { None => "none", Low => "low", Medium => "medium", High => "high", Xhigh => "xhigh", Max => "max" });
string_enum!(Latency { Interactive => "interactive", Balanced => "balanced", Throughput => "throughput" });
string_enum!(Fallback { None => "none", Equivalent => "equivalent", AllowLowerQuality => "allow_lower_quality" });
string_enum!(Modality { Text => "text", Image => "image", Audio => "audio", Video => "video", Json => "json" });
// `quality_fit` prefers the least overqualified candidate that satisfies the
// effective quality floor; `quality` preserves strongest-eligible semantics.
string_enum!(SortKey {
    Placement => "placement",
    DeadlineFit => "deadline_fit",
    QueueTime => "queue_time",
    Cost => "cost",
    QualityFit => "quality_fit",
    Quality => "quality"
});
string_enum!(ProviderProtocol {
    Responses => "responses",
    CodexAppServer => "codex_app_server",
    AudioWorker => "audio_worker",
    Onnx => "onnx"
});
string_enum!(ProviderCapability {
    Responses => "responses",
    Instructions => "instructions",
    Streaming => "streaming",
    FunctionTools => "function_tools",
    ReasoningEffort => "reasoning_effort",
    Temperature => "temperature",
    TopP => "top_p",
    MaxOutputTokens => "max_output_tokens",
    Truncation => "truncation",
    Metadata => "metadata"
});

impl PlacementScope {
    pub fn allows(self, placement: Placement) -> bool {
        match self {
            Self::LocalOnly => placement == Placement::Local,
            Self::Private => placement != Placement::Cloud,
            Self::Anywhere => true,
            Self::CloudOnly => placement == Placement::Cloud,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(model: &str) -> ResponsesRequest {
        ResponsesRequest {
            model: model.into(),
            input: Value::String("text".into()),
            instructions: None,
            stream: false,
            background: false,
            metadata: BTreeMap::new(),
            tools: vec![],
            reasoning: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            truncation: None,
            store: None,
            previous_response_id: None,
            conversation: None,
        }
    }

    #[test]
    fn runtime_metadata_is_never_forwarded() {
        let mut request = request("text.summarize");
        request.metadata = BTreeMap::from([
            ("infer.placement".into(), "local_only".into()),
            ("trace".into(), "x".into()),
        ]);
        assert_eq!(
            request.for_provider("qwen".into()).metadata,
            BTreeMap::from([("trace".into(), "x".into())])
        );
    }

    #[test]
    fn parses_orthogonal_quality_and_placement_constraints() {
        let constraints = RequestConstraints::from_metadata(&BTreeMap::from([
            ("infer.quality_floor".into(), "advanced".into()),
            ("infer.placement".into(), "private".into()),
            ("infer.prefer".into(), "trusted_node".into()),
            ("infer.provider_access_class".into(), "subscription".into()),
        ]))
        .unwrap();
        assert_eq!(constraints.quality_floor, Some(QualityGrade::Advanced));
        assert_eq!(constraints.placement, Some(PlacementScope::Private));
        assert_eq!(constraints.prefer, Some(PlacementPreference::TrustedNode));
        assert_eq!(
            constraints.provider_access_class,
            Some(ProviderAccessClass::Subscription)
        );
    }

    #[test]
    fn stateful_requests_are_rejected() {
        let mut request = request("text.summarize");
        request.previous_response_id = Some("resp_old".into());
        assert_eq!(
            request.validate(),
            Err(ContractError::UnsupportedField("previous_response_id"))
        );
    }

    #[test]
    fn unknown_root_fields_are_rejected_but_reasoning_extensions_are_preserved() {
        let unknown = serde_json::from_value::<ResponsesRequest>(serde_json::json!({
            "model": "text.summarize",
            "input": "text",
            "modle": "typo"
        }))
        .unwrap_err();
        assert!(unknown.to_string().contains("unknown field `modle`"));

        let request = serde_json::from_value::<ResponsesRequest>(serde_json::json!({
            "model": "assistant.general",
            "input": "text",
            "reasoning": {"effort": "low", "provider_mode": "fast"}
        }))
        .unwrap();
        assert_eq!(
            request.reasoning.unwrap().extra["provider_mode"],
            Value::String("fast".into())
        );
    }

    #[test]
    fn execution_requirements_follow_the_public_request_shape() {
        let mut request = request("assistant.general");
        request.instructions = Some(Value::String("be concise".into()));
        request.stream = true;
        request.tools = vec![Value::Null];
        request.reasoning = Some(ReasoningConfig {
            effort: Some(ReasoningEffort::High),
            extra: BTreeMap::new(),
        });
        request.temperature = Some(0.2);
        request.top_p = Some(0.9);
        request.max_output_tokens = Some(100);
        request.truncation = Some("auto".into());
        request.metadata.insert("trace_id".into(), "test".into());
        let requirements = request.execution_requirements();
        assert_eq!(
            requirements.provider_capabilities,
            BTreeSet::from([
                ProviderCapability::Responses,
                ProviderCapability::Instructions,
                ProviderCapability::Streaming,
                ProviderCapability::FunctionTools,
                ProviderCapability::ReasoningEffort,
                ProviderCapability::Temperature,
                ProviderCapability::TopP,
                ProviderCapability::MaxOutputTokens,
                ProviderCapability::Truncation,
                ProviderCapability::Metadata,
            ])
        );
        assert_eq!(
            requirements.model_features,
            BTreeSet::from(["function_tools".into(), "reasoning".into()])
        );
        assert_eq!(requirements.execution_mode, ExecutionMode::ServerStream);
        assert_eq!(
            requirements.input_modalities,
            BTreeSet::from([Modality::Text])
        );
    }

    #[test]
    fn image_parts_are_classified_for_routing_and_cloud_acl() {
        let mut request = request("assistant.multimodal");
        request.input = serde_json::json!([{
            "role": "user",
            "content": [
                {"type": "input_text", "text": "describe"},
                {"type": "input_image", "image_url": "data:image/png;base64,AA=="}
            ]
        }]);
        assert_eq!(
            request.execution_requirements().input_modalities,
            BTreeSet::from([Modality::Text, Modality::Image])
        );
    }

    #[test]
    fn intent_generation_defaults_fill_only_missing_request_fields() {
        let intent: IntentProfile = toml::from_str(
            r#"
            input_modalities = ["text"]
            output_modalities = ["text"]
            default_quality_floor = "basic"
            default_max_output_tokens = 256
            default_reasoning_effort = "none"
            "#,
        )
        .unwrap();

        let mut defaulted = request("text.summarize");
        defaulted.apply_intent_defaults(&intent);
        assert_eq!(defaulted.max_output_tokens, Some(256));
        assert_eq!(defaulted.reasoning_effort(), Some(ReasoningEffort::None));
        let requirements = defaulted.execution_requirements();
        assert!(
            requirements
                .provider_capabilities
                .contains(&ProviderCapability::ReasoningEffort)
        );
        assert!(
            requirements
                .provider_capabilities
                .contains(&ProviderCapability::MaxOutputTokens)
        );
        assert!(!requirements.model_features.contains("reasoning"));

        let mut explicit = request("text.summarize");
        explicit.max_output_tokens = Some(42);
        explicit.reasoning = Some(ReasoningConfig {
            effort: Some(ReasoningEffort::Low),
            extra: BTreeMap::new(),
        });
        explicit.apply_intent_defaults(&intent);
        assert_eq!(explicit.max_output_tokens, Some(42));
        assert_eq!(explicit.reasoning_effort(), Some(ReasoningEffort::Low));
    }
}

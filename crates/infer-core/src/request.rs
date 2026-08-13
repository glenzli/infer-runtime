//! OpenAI Responses-compatible request fields and runtime-only constraints.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ContractError, ExecutionMode, INFER_METADATA_PREFIX, IntentProfile, NamedRouteRequest,
    ProviderAccessClass, parse_ordered_ids, string_enum,
};

pub const IMAGE_GENERATION_INTENT: &str = "image.generate";

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
    /// Standard Responses tool selection for the explicitly supported string
    /// forms. Object-form named tool selection is not part of the frozen Responses capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
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
        self.validate_tools()?;
        let image_generation = self.requests_image_generation();
        if self.model == IMAGE_GENERATION_INTENT && !image_generation {
            return Err(ContractError::UnsupportedField(
                "image.generate without the exact image_generation tool",
            ));
        }
        if self.model != IMAGE_GENERATION_INTENT && image_generation {
            return Err(ContractError::UnsupportedField(
                "image_generation outside image.generate",
            ));
        }
        if image_generation {
            if self.effective_tool_choice() == ToolChoice::None {
                return Err(ContractError::UnsupportedField(
                    "tool_choice=none with image_generation",
                ));
            }
            if self.stream {
                return Err(ContractError::UnsupportedField(
                    "stream=true with image_generation",
                ));
            }
            if self.background {
                return Err(ContractError::UnsupportedField(
                    "background=true with image_generation",
                ));
            }
            if response_input_modalities(&self.input) != BTreeSet::from([Modality::Text]) {
                return Err(ContractError::UnsupportedField(
                    "non-text input with image.generate",
                ));
            }
        }
        RequestConstraints::from_metadata(&self.metadata).map(|_| ())
    }

    fn validate_tools(&self) -> Result<(), ContractError> {
        if self.tools.is_empty() {
            if self.tool_choice.is_some() {
                return Err(ContractError::UnsupportedField("tool_choice without tools"));
            }
            return Ok(());
        }

        let web_search_tools = self
            .tools
            .iter()
            .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("web_search"))
            .collect::<Vec<_>>();
        if web_search_tools.is_empty() {
            return Ok(());
        }
        if self.tools.len() != 1 {
            return Err(ContractError::UnsupportedField(
                "web_search mixed with another tool",
            ));
        }
        let object = web_search_tools[0]
            .as_object()
            .ok_or(ContractError::UnsupportedField("tools.web_search"))?;
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "type" | "external_web_access"))
        {
            return Err(ContractError::UnsupportedField(
                "tools.web_search options outside the frozen subset",
            ));
        }
        if object
            .get("external_web_access")
            .is_some_and(|value| !value.is_boolean())
        {
            return Err(ContractError::UnsupportedField(
                "tools.web_search.external_web_access",
            ));
        }
        Ok(())
    }

    pub fn constraints(&self) -> Result<RequestConstraints, ContractError> {
        RequestConstraints::from_metadata(&self.metadata)
    }

    pub fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.effort)
    }

    /// The only hosted tool currently exposed by a subscription bridge. The
    /// exact single-field shape prevents this capability from becoming a
    /// generic Codex tool escape hatch or silently admitting edit options.
    pub fn requests_image_generation(&self) -> bool {
        let [tool] = self.tools.as_slice() else {
            return false;
        };
        tool.as_object().is_some_and(|object| {
            object.len() == 1
                && object.get("type").and_then(Value::as_str) == Some("image_generation")
        })
    }

    /// Whether the request declares the hosted Responses `web_search` tool.
    /// Validation guarantees it is the sole tool and uses only the bounded
    /// candidate.3 option subset.
    pub fn requests_web_search(&self) -> bool {
        self.tools
            .iter()
            .any(|tool| tool.get("type").and_then(Value::as_str) == Some("web_search"))
    }

    pub fn web_search_external_access(&self) -> Option<bool> {
        self.tools
            .iter()
            .find(|tool| tool.get("type").and_then(Value::as_str) == Some("web_search"))
            .and_then(|tool| tool.get("external_web_access"))
            .and_then(Value::as_bool)
    }

    pub fn effective_tool_choice(&self) -> ToolChoice {
        self.tool_choice.unwrap_or(ToolChoice::Auto)
    }

    /// Whether the declared Web Search tool is eligible to execute. Standard
    /// Responses `tool_choice=none` keeps the declaration but disables the
    /// tool, so it must not require Provider capability or App authority.
    pub fn enables_web_search(&self) -> bool {
        self.requests_web_search() && self.effective_tool_choice() != ToolChoice::None
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
        if !self.tools.is_empty() && self.effective_tool_choice() != ToolChoice::None {
            if self.requests_image_generation() {
                requirements
                    .provider_capabilities
                    .insert(ProviderCapability::ImageGeneration);
                requirements
                    .model_features
                    .insert("image_generation".into());
            } else if self.enables_web_search() {
                requirements
                    .provider_capabilities
                    .insert(ProviderCapability::WebSearch);
                requirements.model_features.insert("web_search".into());
            } else {
                requirements
                    .provider_capabilities
                    .insert(ProviderCapability::FunctionTools);
                requirements.model_features.insert("function_tools".into());
            }
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
    pub capability_floor: Option<CapabilityLevel>,
    pub latency: Option<Latency>,
    pub max_cost_usd: Option<f64>,
    pub fallback: Option<Fallback>,
    pub deadline_ms: Option<u64>,
    /// Optional ordered hard narrowing to Runtime-owned route identities.
    pub named_route: Option<NamedRouteRequest>,
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
                "infer.capability_floor" => {
                    result.capability_floor = Some(value.parse().map_err(|_| {
                        invalid("expected foundational, capable, advanced, expert, or exceptional")
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
                        invalid("expected none, equivalent, or allow_lower_capability")
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
                "infer.deployment_ids" => {
                    if result.named_route.is_some() {
                        return Err(invalid(
                            "deployment_ids and model_profile_ids are mutually exclusive",
                        ));
                    }
                    result.named_route = Some(NamedRouteRequest::Deployments(
                        parse_ordered_ids(value).map_err(&invalid)?,
                    ));
                }
                "infer.model_profile_ids" => {
                    if result.named_route.is_some() {
                        return Err(invalid(
                            "deployment_ids and model_profile_ids are mutually exclusive",
                        ));
                    }
                    result.named_route = Some(NamedRouteRequest::ModelProfiles(
                        parse_ordered_ids(value).map_err(&invalid)?,
                    ));
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
string_enum!(CapabilityLevel {
    Foundational => "foundational",
    Capable => "capable",
    Advanced => "advanced",
    Expert => "expert",
    Exceptional => "exceptional"
});
string_enum!(EvaluationStatus { Provisional => "provisional", Benchmarked => "benchmarked" });
string_enum!(ResourceClass { Light => "light", Standard => "standard", Heavy => "heavy", Extreme => "extreme" });
string_enum!(ReasoningEffort {
    None => "none",
    Low => "low",
    Medium => "medium",
    High => "high",
    Xhigh => "xhigh",
    Max => "max",
    Ultra => "ultra"
});
string_enum!(Latency { Interactive => "interactive", Balanced => "balanced", Throughput => "throughput" });
string_enum!(Fallback { None => "none", Equivalent => "equivalent", AllowLowerCapability => "allow_lower_capability" });
string_enum!(Modality { Text => "text", Image => "image", Audio => "audio", Video => "video", Json => "json" });
// `capability_fit` prefers the least overqualified candidate that satisfies the
// effective capability floor; `capability` preserves strongest-eligible semantics.
string_enum!(SortKey {
    Placement => "placement",
    DeadlineFit => "deadline_fit",
    QueueTime => "queue_time",
    Cost => "cost",
    CapabilityFit => "capability_fit",
    Capability => "capability"
});
string_enum!(ProviderProtocol {
    Responses => "responses",
    CodexAppServer => "codex_app_server",
    AudioWorker => "audio_worker",
    RetrievalWorker => "retrieval_worker",
    OcrWorker => "ocr_worker",
    CoremlWorker => "coreml_worker",
    Onnx => "onnx"
});
string_enum!(ProviderCapability {
    Responses => "responses",
    Instructions => "instructions",
    Streaming => "streaming",
    FunctionTools => "function_tools",
    WebSearch => "web_search",
    ImageGeneration => "image_generation",
    ReasoningEffort => "reasoning_effort",
    Temperature => "temperature",
    TopP => "top_p",
    MaxOutputTokens => "max_output_tokens",
    Truncation => "truncation",
    Metadata => "metadata"
});
string_enum!(ToolChoice {
    None => "none",
    Auto => "auto",
    Required => "required"
});
string_enum!(BuiltinTool {
    WebSearch => "web_search"
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
            tool_choice: None,
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
    fn parses_orthogonal_capability_and_placement_constraints() {
        let constraints = RequestConstraints::from_metadata(&BTreeMap::from([
            ("infer.capability_floor".into(), "expert".into()),
            ("infer.placement".into(), "private".into()),
            ("infer.prefer".into(), "trusted_node".into()),
            ("infer.provider_access_class".into(), "subscription".into()),
        ]))
        .unwrap();
        assert_eq!(constraints.capability_floor, Some(CapabilityLevel::Expert));
        assert_eq!(constraints.placement, Some(PlacementScope::Private));
        assert_eq!(constraints.prefer, Some(PlacementPreference::TrustedNode));
        assert_eq!(
            constraints.provider_access_class,
            Some(ProviderAccessClass::Subscription)
        );
    }

    #[test]
    fn named_route_metadata_is_an_ordered_hard_narrowing() {
        let constraints = RequestConstraints::from_metadata(&BTreeMap::from([(
            "infer.deployment_ids".into(),
            "preferred,backup".into(),
        )]))
        .unwrap();
        assert_eq!(
            constraints.named_route,
            Some(NamedRouteRequest::Deployments(vec![
                "preferred".into(),
                "backup".into()
            ]))
        );
        assert_eq!(
            serde_json::to_value(&constraints).unwrap()["named_route"],
            serde_json::json!({
                "kind": "deployment",
                "ordered_ids": ["preferred", "backup"]
            })
        );
        assert!(
            RequestConstraints::from_metadata(&BTreeMap::from([
                ("infer.deployment_ids".into(), "preferred".into()),
                ("infer.model_profile_ids".into(), "profile".into()),
            ]))
            .is_err()
        );
    }

    #[test]
    fn ultra_reasoning_effort_is_a_public_but_optional_deployment_requirement() {
        let parsed = serde_json::from_value::<ResponsesRequest>(serde_json::json!({
            "model": "reasoning.solve",
            "input": "solve",
            "reasoning": {"effort": "ultra"}
        }))
        .unwrap();
        assert_eq!(parsed.reasoning_effort(), Some(ReasoningEffort::Ultra));
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
            "model": "language.respond",
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
        let mut request = request("language.respond");
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
        let mut request = request("multimodal.respond");
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
    fn image_generation_is_an_exact_dedicated_unary_tool_contract() {
        let mut request = request(IMAGE_GENERATION_INTENT);
        request.tools = vec![serde_json::json!({"type": "image_generation"})];
        request.validate().unwrap();
        assert!(request.requests_image_generation());
        let requirements = request.execution_requirements();
        assert_eq!(
            requirements.provider_capabilities,
            BTreeSet::from([
                ProviderCapability::Responses,
                ProviderCapability::ImageGeneration,
            ])
        );
        assert_eq!(
            requirements.model_features,
            BTreeSet::from(["image_generation".into()])
        );

        let mut wrong_intent = request.clone();
        wrong_intent.model = "language.respond".into();
        assert!(wrong_intent.validate().is_err());

        let mut configured_tool = request.clone();
        configured_tool.tools = vec![serde_json::json!({
            "type": "image_generation",
            "capability": "low"
        })];
        assert!(!configured_tool.requests_image_generation());
        assert!(configured_tool.validate().is_err());

        let mut streaming = request.clone();
        streaming.stream = true;
        assert!(streaming.validate().is_err());

        let mut editing = request;
        editing.input = serde_json::json!({
            "type": "input_image",
            "image_url": "data:image/png;base64,AA=="
        });
        assert!(editing.validate().is_err());
    }

    #[test]
    fn web_search_uses_standard_tool_choice_and_distinct_requirements() {
        let mut request = request("language.respond");
        request.tools = vec![serde_json::json!({
            "type": "web_search",
            "external_web_access": false
        })];
        request.tool_choice = Some(ToolChoice::Required);
        request.validate().unwrap();
        assert!(request.requests_web_search());
        assert_eq!(request.web_search_external_access(), Some(false));
        assert_eq!(request.effective_tool_choice(), ToolChoice::Required);
        assert!(request.enables_web_search());
        let requirements = request.execution_requirements();
        assert!(
            requirements
                .provider_capabilities
                .contains(&ProviderCapability::WebSearch)
        );
        assert!(requirements.model_features.contains("web_search"));
        assert!(!requirements.model_features.contains("function_tools"));

        request.tool_choice = Some(ToolChoice::None);
        request.validate().unwrap();
        assert!(!request.enables_web_search());
        let requirements = request.execution_requirements();
        assert_eq!(
            requirements.provider_capabilities,
            BTreeSet::from([ProviderCapability::Responses])
        );
        assert!(requirements.model_features.is_empty());
    }

    #[test]
    fn web_search_subset_rejects_mixed_or_unbounded_tools() {
        let mut request = request("language.respond");
        request.tools = vec![
            serde_json::json!({"type": "web_search"}),
            serde_json::json!({"type": "function", "name": "escape"}),
        ];
        assert!(matches!(
            request.validate(),
            Err(ContractError::UnsupportedField(
                "web_search mixed with another tool"
            ))
        ));

        request.tools = vec![serde_json::json!({
            "type": "web_search",
            "filters": {"allowed_domains": ["example.com"]}
        })];
        assert!(request.validate().is_err());

        request.tools.clear();
        request.tool_choice = Some(ToolChoice::Required);
        assert!(request.validate().is_err());
    }

    #[test]
    fn intent_generation_defaults_fill_only_missing_request_fields() {
        let intent: IntentProfile = toml::from_str(
            r#"
            input_modalities = ["text"]
            output_modalities = ["text"]
            default_capability_floor = "foundational"
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

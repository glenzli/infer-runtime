//! Capability registry matching and policy-ordered candidate selection.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
};

use infer_core::{
    CandidateDecision, CandidateDecisionStatus, CandidateReasonCode, CapabilityLevel,
    CapabilityRating, EvaluationStatus, ExecutionRequirements, Fallback, IntentProfile, Modality,
    NamedRouteDecision, NamedRouteRequest, Placement, PlacementPreference, PolicyProfile,
    ProviderAccessClass, ReasoningEffort, RequestConstraints, ResourceClass, RoutingDecision,
    RoutingGrantConfig, RuntimeConfig, SortKey,
};

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub deployment_id: String,
    pub provider_id: String,
    pub build_id: String,
    pub model_profile_id: String,
    pub physical_model: String,
    pub placement: Placement,
    pub capability_level: CapabilityLevel,
    pub evaluation_status: EvaluationStatus,
    pub resource_class: ResourceClass,
    /// Admission-time per-Attempt cost estimate. It belongs to the Candidate
    /// so fallback attempts reserve the target deployment's own amount.
    pub estimated_cost_usd: f64,
}

/// An immutable, policy-ordered routing result. `decision` records every
/// configured deployment that was considered, including hard-constraint
/// rejection codes, so later attempts can reuse this admission explanation.
#[derive(Debug, Clone)]
pub struct CandidatePlan {
    pub candidates: Vec<Candidate>,
    pub lower_capability_candidates: Vec<Candidate>,
    pub decision: RoutingDecision,
}

/// Request-dependent inputs to candidate planning. Static registry data stays
/// in `RuntimeConfig`; these values vary for every admitted Job.
pub struct CandidatePlanningContext<'a> {
    pub constraints: &'a RequestConstraints,
    pub execution_requirements: &'a ExecutionRequirements,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub allowed_provider_access_classes: &'a BTreeSet<ProviderAccessClass>,
    pub allowed_cloud_input_modalities: &'a BTreeSet<Modality>,
    /// Applicable App execution grant. An Intent-specific rule has already
    /// replaced the App-global rule before candidate planning.
    pub routing_grant: Option<&'a RoutingGrantConfig>,
    pub unavailable_providers: &'a BTreeSet<String>,
    /// Last known local resource inventory failures. Provider health and
    /// resource inventory intentionally remain separate: a provider can be
    /// reachable while one configured build is not installed.
    pub unavailable_deployments: &'a BTreeSet<String>,
}

/// A bounded, best-effort scheduling observation captured immediately before
/// routing. It is deliberately not persisted as Job provenance: queue state
/// can change before admission, while the resulting ordered Candidate Plan is
/// the durable decision record.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderQueueEstimate {
    pub estimated_wait_ms: Option<u64>,
    pub estimated_service_ms: Option<u64>,
}

#[cfg(test)]
pub fn plan_candidates(
    config: &RuntimeConfig,
    intent_id: &str,
    intent: &IntentProfile,
    profile: &PolicyProfile,
    context: CandidatePlanningContext<'_>,
) -> CandidatePlan {
    plan_candidates_with_queue(
        config,
        intent_id,
        intent,
        profile,
        context,
        &BTreeMap::new(),
    )
}

/// Equivalent static planning surface used by tests and offline tools, with a
/// bounded live queue estimate available to the Runtime admission path.
pub fn plan_candidates_with_queue(
    config: &RuntimeConfig,
    intent_id: &str,
    intent: &IntentProfile,
    profile: &PolicyProfile,
    context: CandidatePlanningContext<'_>,
    queue_estimates: &BTreeMap<String, ProviderQueueEstimate>,
) -> CandidatePlan {
    let CandidatePlanningContext {
        constraints,
        execution_requirements,
        reasoning_effort,
        allowed_provider_access_classes,
        allowed_cloud_input_modalities,
        routing_grant,
        unavailable_providers,
        unavailable_deployments,
    } = context;
    let capability_floor = constraints
        .capability_floor
        .map_or(intent.default_capability_floor, |requested| {
            requested.max(intent.default_capability_floor)
        });
    let placement_scope = constraints.placement;
    let mut eligible = Vec::new();
    let mut lower_capability = Vec::new();
    let mut decisions = Vec::new();
    for (deployment_id, deployment) in &config.deployments {
        let provider = &config.providers[&deployment.provider];
        let build = &config.model_builds[&deployment.build];
        let model = &config.model_profiles[&build.profile];
        let Some(rating) = model.ratings.get(intent_id) else {
            decisions.push(CandidateDecision {
                deployment: deployment_id.clone(),
                provider: deployment.provider.clone(),
                status: CandidateDecisionStatus::Rejected,
                rank: None,
                reason_codes: vec![CandidateReasonCode::IntentUnassessed],
            });
            continue;
        };
        let mut reason_codes = Vec::new();
        let named_target_is_explicitly_authorized = constraints
            .named_route
            .as_ref()
            .is_some_and(|target| target.rank(deployment_id, &build.profile).is_some())
            && routing_grant
                .is_some_and(|grant| grant.allows_named_deployment(deployment_id, &build.profile));
        if (provider.kind == infer_core::ProviderKind::TrustedNode && routing_grant.is_none())
            || routing_grant.is_some_and(|grant| {
                !grant.allows_deployment(deployment_id, &build.profile)
                    && !named_target_is_explicitly_authorized
            })
        {
            reason_codes.push(CandidateReasonCode::RoutingGrantExcluded);
        }
        if let Some(target) = constraints.named_route.as_ref() {
            match target.rank(deployment_id, &build.profile) {
                None => reason_codes.push(CandidateReasonCode::NamedRouteMismatch),
                Some(rank)
                    if rank > 0
                        && constraints.fallback.unwrap_or(Fallback::None) == Fallback::None =>
                {
                    reason_codes.push(CandidateReasonCode::NamedRouteFallbackDisabled);
                }
                Some(_) => {}
            }
        }
        if !allowed_provider_access_classes.contains(&provider.access_class) {
            reason_codes.push(CandidateReasonCode::ProviderAccessNotAllowed);
        }
        if constraints
            .provider_access_class
            .is_some_and(|requested| requested != provider.access_class)
        {
            reason_codes.push(CandidateReasonCode::ProviderAccessClassMismatch);
        }
        if provider.placement == Placement::Cloud
            && !execution_requirements
                .input_modalities
                .is_subset(allowed_cloud_input_modalities)
        {
            reason_codes.push(CandidateReasonCode::CloudInputModalityNotAllowed);
        }
        if !provider.is_configured() {
            reason_codes.push(CandidateReasonCode::ProviderCredentialUnavailable);
        }
        if unavailable_providers.contains(&deployment.provider) {
            reason_codes.push(CandidateReasonCode::ProviderCircuitOpen);
        }
        if unavailable_deployments.contains(deployment_id) {
            reason_codes.push(CandidateReasonCode::DeploymentUnavailable);
        }
        if !execution_requirements
            .provider_capabilities
            .iter()
            .all(|capability| provider.capability_profile.supports(*capability))
        {
            reason_codes.push(CandidateReasonCode::ProviderCapabilityMissing);
        }
        let responses_stream_compatibility = execution_requirements.execution_mode
            == infer_core::ExecutionMode::ServerStream
            && matches!(
                provider.capability_profile.protocol,
                infer_core::ProviderProtocol::Responses
                    | infer_core::ProviderProtocol::CodexAppServer
            );
        if !responses_stream_compatibility
            && !deployment
                .supported_execution_modes
                .contains(&execution_requirements.execution_mode)
        {
            reason_codes.push(CandidateReasonCode::ExecutionModeUnsupported);
        }
        if !intent
            .input_modalities
            .iter()
            .chain(execution_requirements.input_modalities.iter())
            .all(|modality| build.input_modalities.contains(modality))
        {
            reason_codes.push(CandidateReasonCode::InputModalityMissing);
        }
        if !intent
            .output_modalities
            .iter()
            .all(|modality| build.output_modalities.contains(modality))
        {
            reason_codes.push(CandidateReasonCode::OutputModalityMissing);
        }
        if !intent
            .required_features
            .iter()
            .chain(execution_requirements.model_features.iter())
            .all(|feature| build.features.contains(feature))
        {
            reason_codes.push(CandidateReasonCode::RequiredFeatureMissing);
        }
        if placement_scope.is_some_and(|scope| !scope.allows(provider.placement)) {
            reason_codes.push(CandidateReasonCode::PlacementNotAllowed);
        }
        if constraints.offline_required == Some(true) && provider.placement == Placement::Cloud {
            reason_codes.push(CandidateReasonCode::OfflineRequired);
        }
        if rating.level < capability_floor {
            reason_codes.push(CandidateReasonCode::CapabilityBelowFloor);
        }
        if reasoning_effort.is_some_and(|effort| !deployment.supported_efforts.contains(&effort)) {
            reason_codes.push(CandidateReasonCode::ReasoningEffortUnsupported);
        }
        if constraints
            .max_cost_usd
            .is_some_and(|limit| deployment.estimated_cost_usd > limit)
        {
            reason_codes.push(CandidateReasonCode::CostLimitExceeded);
        }

        let candidate = Candidate {
            deployment_id: deployment_id.clone(),
            provider_id: deployment.provider.clone(),
            build_id: deployment.build.clone(),
            model_profile_id: build.profile.clone(),
            physical_model: build.model_id.clone(),
            placement: provider.placement,
            capability_level: rating.level,
            evaluation_status: rating.status,
            resource_class: deployment.resource_class,
            estimated_cost_usd: deployment.estimated_cost_usd,
        };
        let fallback_eligible = constraints.fallback == Some(Fallback::AllowLowerCapability)
            && capability_floor > intent.default_capability_floor
            && rating.level >= intent.default_capability_floor
            && reason_codes == vec![CandidateReasonCode::CapabilityBelowFloor];
        let decision = CandidateDecision {
            deployment: deployment_id.clone(),
            provider: deployment.provider.clone(),
            status: if reason_codes.is_empty() {
                CandidateDecisionStatus::Eligible
            } else if fallback_eligible {
                CandidateDecisionStatus::FallbackEligible
            } else {
                CandidateDecisionStatus::Rejected
            },
            rank: None,
            reason_codes,
        };
        if decision.status == CandidateDecisionStatus::Eligible {
            eligible.push((
                (deployment_id, deployment, provider, build, model, rating),
                candidate,
                decision,
            ));
        } else if decision.status == CandidateDecisionStatus::FallbackEligible {
            lower_capability.push((
                (deployment_id, deployment, provider, build, model, rating),
                candidate,
                decision,
            ));
        } else {
            decisions.push(decision);
        }
    }

    eligible.sort_by(|left, right| {
        named_rank(constraints.named_route.as_ref(), &left.1)
            .cmp(&named_rank(constraints.named_route.as_ref(), &right.1))
            .then_with(|| {
                compare_candidates(
                    &left.0,
                    &right.0,
                    &profile.order,
                    constraints.prefer,
                    capability_floor,
                    constraints.deadline_ms,
                    queue_estimates,
                )
            })
    });
    let candidates = eligible
        .into_iter()
        .enumerate()
        .map(|(index, (_reference, candidate, mut decision))| {
            decision.rank = Some(index + 1);
            decisions.push(decision);
            candidate
        })
        .collect();
    lower_capability.sort_by(|left, right| {
        named_rank(constraints.named_route.as_ref(), &left.1)
            .cmp(&named_rank(constraints.named_route.as_ref(), &right.1))
            .then_with(|| {
                compare_candidates(
                    &left.0,
                    &right.0,
                    &profile.order,
                    constraints.prefer,
                    capability_floor,
                    constraints.deadline_ms,
                    queue_estimates,
                )
            })
    });
    let lower_capability_candidates = lower_capability
        .into_iter()
        .enumerate()
        .map(|(index, (_reference, candidate, mut decision))| {
            decision.rank = Some(index + 1);
            decisions.push(decision);
            candidate
        })
        .collect();
    CandidatePlan {
        candidates,
        lower_capability_candidates,
        decision: RoutingDecision {
            capability_floor,
            named_route: constraints.named_route.as_ref().map(|target| match target {
                NamedRouteRequest::Deployments(ids) => NamedRouteDecision {
                    kind: "deployment".into(),
                    ordered_ids: ids.clone(),
                },
                NamedRouteRequest::ModelProfiles(ids) => NamedRouteDecision {
                    kind: "model_profile".into(),
                    ordered_ids: ids.clone(),
                },
            }),
            candidates: decisions,
        },
    }
}

fn named_rank(target: Option<&NamedRouteRequest>, candidate: &Candidate) -> usize {
    target
        .and_then(|target| target.rank(&candidate.deployment_id, &candidate.model_profile_id))
        .unwrap_or(usize::MAX)
}

/// Rehydrates one admission-time deployment from the same immutable config
/// fingerprint during durable local recovery.
pub fn candidate_for_deployment(
    config: &RuntimeConfig,
    intent_id: &str,
    deployment_id: &str,
) -> Option<Candidate> {
    let deployment = config.deployments.get(deployment_id)?;
    let provider = config.providers.get(&deployment.provider)?;
    let build = config.model_builds.get(&deployment.build)?;
    let model = config.model_profiles.get(&build.profile)?;
    let rating = model.ratings.get(intent_id)?;
    Some(Candidate {
        deployment_id: deployment_id.into(),
        provider_id: deployment.provider.clone(),
        build_id: deployment.build.clone(),
        model_profile_id: build.profile.clone(),
        physical_model: build.model_id.clone(),
        placement: provider.placement,
        capability_level: rating.level,
        evaluation_status: rating.status,
        resource_class: deployment.resource_class,
        estimated_cost_usd: deployment.estimated_cost_usd,
    })
}

#[cfg(test)]
pub fn select_candidate(
    config: &RuntimeConfig,
    intent_id: &str,
    intent: &IntentProfile,
    profile: &PolicyProfile,
    constraints: &RequestConstraints,
    reasoning_effort: Option<ReasoningEffort>,
) -> Option<Candidate> {
    plan_candidates(
        config,
        intent_id,
        intent,
        profile,
        CandidatePlanningContext {
            constraints,
            execution_requirements: &ExecutionRequirements::default(),
            reasoning_effort,
            allowed_provider_access_classes: &BTreeSet::from([ProviderAccessClass::Standard]),
            allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text]),
            routing_grant: None,
            unavailable_providers: &BTreeSet::new(),
            unavailable_deployments: &BTreeSet::new(),
        },
    )
    .candidates
    .into_iter()
    .next()
}

type CandidateRef<'a> = (
    &'a String,
    &'a infer_core::DeploymentConfig,
    &'a infer_core::ProviderConfig,
    &'a infer_core::ModelBuildConfig,
    &'a infer_core::ModelProfileConfig,
    &'a CapabilityRating,
);

fn compare_candidates(
    left: &CandidateRef<'_>,
    right: &CandidateRef<'_>,
    order: &[SortKey],
    preferred: Option<PlacementPreference>,
    capability_floor: CapabilityLevel,
    deadline_ms: Option<u64>,
    queue_estimates: &BTreeMap<String, ProviderQueueEstimate>,
) -> Ordering {
    for key in order {
        let comparison = match key {
            SortKey::Placement => placement_rank(left.2.placement, preferred)
                .cmp(&placement_rank(right.2.placement, preferred)),
            SortKey::Cost => left
                .1
                .estimated_cost_usd
                .partial_cmp(&right.1.estimated_cost_usd)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.1.resource_class.cmp(&right.1.resource_class)),
            SortKey::Capability => right
                .5
                .level
                .cmp(&left.5.level)
                .then_with(|| compare_score(right.5.score, left.5.score)),
            SortKey::CapabilityFit => capability_distance(left.5.level, capability_floor)
                .cmp(&capability_distance(right.5.level, capability_floor))
                .then_with(|| compare_score(right.5.score, left.5.score)),
            SortKey::DeadlineFit => deadline_fit_rank(left, deadline_ms, queue_estimates)
                .cmp(&deadline_fit_rank(right, deadline_ms, queue_estimates)),
            SortKey::QueueTime => {
                queue_wait_ms(left, queue_estimates).cmp(&queue_wait_ms(right, queue_estimates))
            }
        };
        if comparison != Ordering::Equal {
            return comparison;
        }
    }
    left.0.cmp(right.0)
}

fn queue_wait_ms(
    candidate: &CandidateRef<'_>,
    queue_estimates: &BTreeMap<String, ProviderQueueEstimate>,
) -> u64 {
    queue_estimates
        .get(&candidate.1.provider)
        .and_then(|estimate| estimate.estimated_wait_ms)
        .unwrap_or(u64::MAX)
}

/// Prefer a candidate with a measured queue delay that fits the caller's
/// deadline. Unknown timing stays neutral rather than being treated as an
/// optimistic zero; a known miss ranks behind unknown but does not alter any
/// hard deadline/cancellation behavior at execution time.
fn deadline_fit_rank(
    candidate: &CandidateRef<'_>,
    deadline_ms: Option<u64>,
    queue_estimates: &BTreeMap<String, ProviderQueueEstimate>,
) -> u8 {
    let Some(deadline_ms) = deadline_ms else {
        return 0;
    };
    let Some(estimate) = queue_estimates.get(&candidate.1.provider) else {
        return 1;
    };
    let Some(wait_ms) = estimate.estimated_wait_ms else {
        return 1;
    };
    let Some(service_ms) = estimate.estimated_service_ms else {
        return 1;
    };
    if wait_ms.saturating_add(service_ms) <= deadline_ms {
        0
    } else {
        2
    }
}

fn capability_distance(level: CapabilityLevel, floor: CapabilityLevel) -> u8 {
    capability_rank(level).abs_diff(capability_rank(floor))
}

fn capability_rank(level: CapabilityLevel) -> u8 {
    match level {
        CapabilityLevel::Foundational => 0,
        CapabilityLevel::Capable => 1,
        CapabilityLevel::Advanced => 2,
        CapabilityLevel::Expert => 3,
        CapabilityLevel::Exceptional => 4,
    }
}

fn compare_score(left: Option<f64>, right: Option<f64>) -> Ordering {
    left.unwrap_or_default()
        .partial_cmp(&right.unwrap_or_default())
        .unwrap_or(Ordering::Equal)
}

fn placement_rank(placement: Placement, preferred: Option<PlacementPreference>) -> u8 {
    let preferred_placement = preferred.map(|preference| match preference {
        PlacementPreference::Local => Placement::Local,
        PlacementPreference::TrustedNode => Placement::TrustedNode,
        PlacementPreference::Cloud => Placement::Cloud,
    });
    if preferred_placement == Some(placement) {
        return 0;
    }
    match placement {
        Placement::Local => 1,
        Placement::TrustedNode => 2,
        Placement::Cloud => 3,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn config() -> RuntimeConfig {
        toml::from_str(
            r#"
            [server]
            bind = "127.0.0.1:8787"
            [defaults]
            policy = "balanced"
            [profiles.balanced]
            order = ["capability", "placement"]
            [providers.local]
            kind = "responses"
            base_url = "http://local/v1"
            placement = "local"
            [providers.local.capability_profile]
            version = 1
            protocol = "responses"
            capabilities = ["responses"]
            [providers.cloud]
            kind = "responses"
            base_url = "https://cloud/v1"
            placement = "cloud"
            [providers.cloud.capability_profile]
            version = 1
            protocol = "responses"
            capabilities = ["responses", "function_tools"]
            [intents."reasoning.solve"]
            input_modalities = ["text"]
            output_modalities = ["text"]
            default_capability_floor = "expert"
            [model_profiles.small]
            family = "small"
            [model_profiles.small.ratings."reasoning.solve"]
            level = "foundational"
            status = "provisional"
            [model_profiles.strong]
            family = "strong"
            [model_profiles.strong.ratings."reasoning.solve"]
            level = "expert"
            status = "benchmarked"
            eval_profile = "reasoning-v1"
            score = 0.8
            [model_builds.small_local]
            profile = "small"
            model_id = "small"
            input_modalities = ["text"]
            output_modalities = ["text"]
            [model_builds.strong_cloud]
            profile = "strong"
            model_id = "strong"
            input_modalities = ["text"]
            output_modalities = ["text"]
            features = ["function_tools"]
            [deployments.small_local]
            provider = "local"
            build = "small_local"
            [deployments.strong_cloud]
            provider = "cloud"
            build = "strong_cloud"
            [apps.test]
            credential = { source = "environment", variable = "INFER_TEST_TOKEN" }
            allowed_intents = ["reasoning.solve"]
            "#,
        )
        .unwrap()
    }

    #[test]
    fn capability_floor_excludes_a_weak_local_model() {
        let config = config();
        config.validate().unwrap();
        let intent = config.intent("reasoning.solve").unwrap();
        let plan = plan_candidates(
            &config,
            "reasoning.solve",
            intent,
            &config.profiles["balanced"],
            CandidatePlanningContext {
                constraints: &RequestConstraints::default(),
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: None,
                allowed_provider_access_classes: &BTreeSet::from([ProviderAccessClass::Standard]),
                allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text]),
                routing_grant: None,
                unavailable_providers: &BTreeSet::new(),
                unavailable_deployments: &BTreeSet::new(),
            },
        );
        let selected = plan.candidates.first().unwrap();
        assert_eq!(selected.deployment_id, "strong_cloud");
        let rejected = plan
            .decision
            .candidates
            .iter()
            .find(|candidate| candidate.deployment == "small_local")
            .unwrap();
        assert_eq!(rejected.status, CandidateDecisionStatus::Rejected);
        assert_eq!(
            rejected.reason_codes,
            vec![CandidateReasonCode::CapabilityBelowFloor]
        );
    }

    #[test]
    fn a_trusted_node_requires_an_explicit_app_routing_grant() {
        let mut config = config();
        let provider = config.providers.get_mut("cloud").unwrap();
        provider.kind = infer_core::ProviderKind::TrustedNode;
        provider.placement = Placement::TrustedNode;
        let grant = RoutingGrantConfig {
            deployment_ids: BTreeSet::from(["strong_cloud".into()]),
            ..Default::default()
        };
        for (routing_grant, expected) in [(None, 0), (Some(&grant), 1)] {
            let plan = plan_candidates(
                &config,
                "reasoning.solve",
                config.intent("reasoning.solve").unwrap(),
                &config.profiles["balanced"],
                CandidatePlanningContext {
                    constraints: &RequestConstraints::default(),
                    execution_requirements: &ExecutionRequirements::default(),
                    reasoning_effort: None,
                    allowed_provider_access_classes: &BTreeSet::from([
                        ProviderAccessClass::Standard,
                    ]),
                    allowed_cloud_input_modalities: &BTreeSet::new(),
                    routing_grant,
                    unavailable_providers: &BTreeSet::new(),
                    unavailable_deployments: &BTreeSet::new(),
                },
            );
            assert_eq!(plan.candidates.len(), expected);
            if expected == 0 {
                assert!(plan.decision.candidates.iter().any(|candidate| {
                    candidate.deployment == "strong_cloud"
                        && candidate
                            .reason_codes
                            .contains(&CandidateReasonCode::RoutingGrantExcluded)
                }));
            }
        }
    }

    #[test]
    fn missing_intent_rating_is_unassessed_instead_of_unsupported() {
        let mut config = config();
        config
            .model_profiles
            .get_mut("strong")
            .unwrap()
            .ratings
            .remove("reasoning.solve");
        config.validate().unwrap();
        let intent = config.intent("reasoning.solve").unwrap();
        let plan = plan_candidates(
            &config,
            "reasoning.solve",
            intent,
            &config.profiles["balanced"],
            CandidatePlanningContext {
                constraints: &RequestConstraints::default(),
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: None,
                allowed_provider_access_classes: &BTreeSet::from([ProviderAccessClass::Standard]),
                allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text]),
                routing_grant: None,
                unavailable_providers: &BTreeSet::new(),
                unavailable_deployments: &BTreeSet::new(),
            },
        );

        assert!(plan.candidates.is_empty());
        let unassessed = plan
            .decision
            .candidates
            .iter()
            .find(|candidate| candidate.deployment == "strong_cloud")
            .unwrap();
        assert_eq!(unassessed.status, CandidateDecisionStatus::Rejected);
        assert_eq!(
            unassessed.reason_codes,
            vec![CandidateReasonCode::IntentUnassessed]
        );
    }

    #[test]
    fn subscription_provider_requires_an_explicit_app_entitlement() {
        let mut config = config();
        config.providers.get_mut("cloud").unwrap().access_class = ProviderAccessClass::Subscription;
        let intent = config.intent("reasoning.solve").unwrap();
        let denied = plan_candidates(
            &config,
            "reasoning.solve",
            intent,
            &config.profiles["balanced"],
            CandidatePlanningContext {
                constraints: &RequestConstraints::default(),
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: None,
                allowed_provider_access_classes: &BTreeSet::from([ProviderAccessClass::Standard]),
                allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text]),
                routing_grant: None,
                unavailable_providers: &BTreeSet::new(),
                unavailable_deployments: &BTreeSet::new(),
            },
        );
        assert!(denied.candidates.is_empty());
        assert!(denied.decision.candidates.iter().any(|candidate| {
            candidate
                .reason_codes
                .contains(&CandidateReasonCode::ProviderAccessNotAllowed)
        }));

        let admitted = plan_candidates(
            &config,
            "reasoning.solve",
            intent,
            &config.profiles["balanced"],
            CandidatePlanningContext {
                constraints: &RequestConstraints::default(),
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: None,
                allowed_provider_access_classes: &BTreeSet::from([
                    ProviderAccessClass::Standard,
                    ProviderAccessClass::Subscription,
                ]),
                allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text]),
                routing_grant: None,
                unavailable_providers: &BTreeSet::new(),
                unavailable_deployments: &BTreeSet::new(),
            },
        );
        assert_eq!(admitted.candidates[0].provider_id, "cloud");
    }

    #[test]
    fn request_can_narrow_an_authorized_app_to_one_provider_access_class() {
        let mut config = config();
        config.providers.get_mut("cloud").unwrap().access_class = ProviderAccessClass::Subscription;
        config
            .model_profiles
            .get_mut("small")
            .unwrap()
            .ratings
            .get_mut("reasoning.solve")
            .unwrap()
            .level = CapabilityLevel::Expert;
        let intent = config.intent("reasoning.solve").unwrap();
        let allowed = BTreeSet::from([
            ProviderAccessClass::Standard,
            ProviderAccessClass::Subscription,
        ]);
        let plan = plan_candidates(
            &config,
            "reasoning.solve",
            intent,
            &config.profiles["balanced"],
            CandidatePlanningContext {
                constraints: &RequestConstraints {
                    provider_access_class: Some(ProviderAccessClass::Subscription),
                    ..RequestConstraints::default()
                },
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: None,
                allowed_provider_access_classes: &allowed,
                allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text]),
                routing_grant: None,
                unavailable_providers: &BTreeSet::new(),
                unavailable_deployments: &BTreeSet::new(),
            },
        );
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].provider_id, "cloud");
        assert!(plan.decision.candidates.iter().any(|candidate| {
            candidate.deployment == "small_local"
                && candidate
                    .reason_codes
                    .contains(&CandidateReasonCode::ProviderAccessClassMismatch)
        }));
    }

    #[test]
    fn capability_fit_chooses_the_minimum_sufficient_level() {
        let mut config = config();
        config
            .intents
            .get_mut("reasoning.solve")
            .unwrap()
            .default_capability_floor = CapabilityLevel::Capable;
        config
            .model_profiles
            .get_mut("small")
            .unwrap()
            .ratings
            .get_mut("reasoning.solve")
            .unwrap()
            .level = CapabilityLevel::Capable;
        let intent = config.intent("reasoning.solve").unwrap();
        let constraints = RequestConstraints::default();
        let allowed = BTreeSet::from([ProviderAccessClass::Standard]);
        let cloud_inputs = BTreeSet::from([Modality::Text]);
        let capability_fit = PolicyProfile {
            order: vec![SortKey::CapabilityFit],
        };
        let strongest = PolicyProfile {
            order: vec![SortKey::Capability],
        };

        let fitted = plan_candidates(
            &config,
            "reasoning.solve",
            intent,
            &capability_fit,
            CandidatePlanningContext {
                constraints: &constraints,
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: None,
                allowed_provider_access_classes: &allowed,
                allowed_cloud_input_modalities: &cloud_inputs,
                routing_grant: None,
                unavailable_providers: &BTreeSet::new(),
                unavailable_deployments: &BTreeSet::new(),
            },
        );
        assert_eq!(fitted.candidates[0].deployment_id, "small_local");

        let strongest = plan_candidates(
            &config,
            "reasoning.solve",
            intent,
            &strongest,
            CandidatePlanningContext {
                constraints: &constraints,
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: None,
                allowed_provider_access_classes: &allowed,
                allowed_cloud_input_modalities: &cloud_inputs,
                routing_grant: None,
                unavailable_providers: &BTreeSet::new(),
                unavailable_deployments: &BTreeSet::new(),
            },
        );
        assert_eq!(strongest.candidates[0].deployment_id, "strong_cloud");
    }

    #[test]
    fn measured_queue_time_reorders_equally_eligible_candidates() {
        let mut config = config();
        config
            .model_profiles
            .get_mut("small")
            .unwrap()
            .ratings
            .get_mut("reasoning.solve")
            .unwrap()
            .level = CapabilityLevel::Expert;
        let constraints = RequestConstraints::default();
        let queue_estimates = BTreeMap::from([
            (
                "local".into(),
                ProviderQueueEstimate {
                    estimated_wait_ms: Some(20),
                    estimated_service_ms: Some(20),
                },
            ),
            (
                "cloud".into(),
                ProviderQueueEstimate {
                    estimated_wait_ms: Some(500),
                    estimated_service_ms: Some(100),
                },
            ),
        ]);
        let plan = plan_candidates_with_queue(
            &config,
            "reasoning.solve",
            config.intent("reasoning.solve").unwrap(),
            &PolicyProfile {
                order: vec![SortKey::QueueTime],
            },
            CandidatePlanningContext {
                constraints: &constraints,
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: None,
                allowed_provider_access_classes: &BTreeSet::from([ProviderAccessClass::Standard]),
                allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text]),
                routing_grant: None,
                unavailable_providers: &BTreeSet::new(),
                unavailable_deployments: &BTreeSet::new(),
            },
            &queue_estimates,
        );
        assert_eq!(plan.candidates[0].deployment_id, "small_local");
        assert_eq!(plan.candidates[1].deployment_id, "strong_cloud");
    }

    #[test]
    fn deadline_fit_prefers_a_measured_candidate_that_can_start_in_time() {
        let mut config = config();
        config
            .model_profiles
            .get_mut("small")
            .unwrap()
            .ratings
            .get_mut("reasoning.solve")
            .unwrap()
            .level = CapabilityLevel::Expert;
        let constraints = RequestConstraints {
            deadline_ms: Some(100),
            ..RequestConstraints::default()
        };
        let queue_estimates = BTreeMap::from([
            (
                "local".into(),
                ProviderQueueEstimate {
                    estimated_wait_ms: Some(20),
                    estimated_service_ms: Some(20),
                },
            ),
            (
                "cloud".into(),
                ProviderQueueEstimate {
                    estimated_wait_ms: Some(80),
                    estimated_service_ms: Some(80),
                },
            ),
        ]);
        let plan = plan_candidates_with_queue(
            &config,
            "reasoning.solve",
            config.intent("reasoning.solve").unwrap(),
            &PolicyProfile {
                order: vec![SortKey::DeadlineFit, SortKey::Capability],
            },
            CandidatePlanningContext {
                constraints: &constraints,
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: None,
                allowed_provider_access_classes: &BTreeSet::from([ProviderAccessClass::Standard]),
                allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text]),
                routing_grant: None,
                unavailable_providers: &BTreeSet::new(),
                unavailable_deployments: &BTreeSet::new(),
            },
            &queue_estimates,
        );
        assert_eq!(plan.candidates[0].deployment_id, "small_local");
    }

    #[test]
    fn cloud_image_egress_requires_an_independent_app_entitlement() {
        let mut config = config();
        config
            .model_builds
            .get_mut("strong_cloud")
            .unwrap()
            .input_modalities
            .push(Modality::Image);
        let intent = config.intent("reasoning.solve").unwrap();
        let requirements = ExecutionRequirements {
            input_modalities: BTreeSet::from([Modality::Text, Modality::Image]),
            ..ExecutionRequirements::default()
        };
        let denied = plan_candidates(
            &config,
            "reasoning.solve",
            intent,
            &config.profiles["balanced"],
            CandidatePlanningContext {
                constraints: &RequestConstraints::default(),
                execution_requirements: &requirements,
                reasoning_effort: None,
                allowed_provider_access_classes: &BTreeSet::from([ProviderAccessClass::Standard]),
                allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text]),
                routing_grant: None,
                unavailable_providers: &BTreeSet::new(),
                unavailable_deployments: &BTreeSet::new(),
            },
        );
        assert!(denied.candidates.is_empty());
        assert!(denied.decision.candidates.iter().any(|candidate| {
            candidate
                .reason_codes
                .contains(&CandidateReasonCode::CloudInputModalityNotAllowed)
        }));

        let admitted = plan_candidates(
            &config,
            "reasoning.solve",
            intent,
            &config.profiles["balanced"],
            CandidatePlanningContext {
                constraints: &RequestConstraints::default(),
                execution_requirements: &requirements,
                reasoning_effort: None,
                allowed_provider_access_classes: &BTreeSet::from([ProviderAccessClass::Standard]),
                allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text, Modality::Image]),
                routing_grant: None,
                unavailable_providers: &BTreeSet::new(),
                unavailable_deployments: &BTreeSet::new(),
            },
        );
        assert_eq!(admitted.candidates[0].deployment_id, "strong_cloud");
    }

    #[test]
    fn local_only_does_not_cross_the_capability_or_placement_boundary() {
        let config = config();
        let selected = select_candidate(
            &config,
            "reasoning.solve",
            config.intent("reasoning.solve").unwrap(),
            &config.profiles["balanced"],
            &RequestConstraints {
                placement: Some(infer_core::PlacementScope::LocalOnly),
                ..Default::default()
            },
            None,
        );
        assert!(selected.is_none());
    }

    #[test]
    fn explicit_capability_fallback_never_drops_below_the_intent_floor() {
        let config = config();
        let plan = plan_candidates(
            &config,
            "reasoning.solve",
            config.intent("reasoning.solve").unwrap(),
            &config.profiles["balanced"],
            CandidatePlanningContext {
                constraints: &RequestConstraints {
                    capability_floor: Some(CapabilityLevel::Exceptional),
                    fallback: Some(Fallback::AllowLowerCapability),
                    ..Default::default()
                },
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: None,
                allowed_provider_access_classes: &BTreeSet::from([ProviderAccessClass::Standard]),
                allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text]),
                routing_grant: None,
                unavailable_providers: &BTreeSet::new(),
                unavailable_deployments: &BTreeSet::new(),
            },
        );
        assert!(plan.candidates.is_empty());
        assert_eq!(plan.lower_capability_candidates.len(), 1);
        assert_eq!(
            plan.lower_capability_candidates[0].deployment_id,
            "strong_cloud"
        );
        assert!(plan.lower_capability_candidates[0].capability_level >= CapabilityLevel::Expert);
    }

    #[test]
    fn request_capabilities_filter_candidates_before_dispatch() {
        let config = config();
        let plan = plan_candidates(
            &config,
            "reasoning.solve",
            config.intent("reasoning.solve").unwrap(),
            &config.profiles["balanced"],
            CandidatePlanningContext {
                constraints: &RequestConstraints::default(),
                execution_requirements: &ExecutionRequirements {
                    provider_capabilities: BTreeSet::from([
                        infer_core::ProviderCapability::FunctionTools,
                    ]),
                    model_features: BTreeSet::from(["function_tools".into()]),
                    ..ExecutionRequirements::default()
                },
                reasoning_effort: None,
                allowed_provider_access_classes: &BTreeSet::from([ProviderAccessClass::Standard]),
                allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text]),
                routing_grant: None,
                unavailable_providers: &BTreeSet::new(),
                unavailable_deployments: &BTreeSet::new(),
            },
        );
        assert_eq!(plan.candidates[0].deployment_id, "strong_cloud");
        let local = plan
            .decision
            .candidates
            .iter()
            .find(|candidate| candidate.deployment == "small_local")
            .unwrap();
        assert!(
            local
                .reason_codes
                .contains(&CandidateReasonCode::ProviderCapabilityMissing)
        );
    }

    #[test]
    fn unavailable_deployment_is_recorded_without_opening_its_provider_circuit() {
        let config = config();
        let unavailable = BTreeSet::from(["small_local".into()]);
        let plan = plan_candidates(
            &config,
            "reasoning.solve",
            config.intent("reasoning.solve").unwrap(),
            &config.profiles["balanced"],
            CandidatePlanningContext {
                constraints: &RequestConstraints::default(),
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: None,
                allowed_provider_access_classes: &BTreeSet::from([ProviderAccessClass::Standard]),
                allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text]),
                routing_grant: None,
                unavailable_providers: &BTreeSet::new(),
                unavailable_deployments: &unavailable,
            },
        );
        let local = plan
            .decision
            .candidates
            .iter()
            .find(|candidate| candidate.deployment == "small_local")
            .unwrap();
        assert!(
            local
                .reason_codes
                .contains(&CandidateReasonCode::DeploymentUnavailable)
        );
        assert!(
            !local
                .reason_codes
                .contains(&CandidateReasonCode::ProviderCircuitOpen)
        );
    }

    #[test]
    fn example_registry_routes_cloud_only_when_policy_allows_it() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut config = RuntimeConfig::load(path).unwrap();
        config
            .providers
            .get_mut("deepseek-cloud")
            .unwrap()
            .api_key_env = Some("INFER_RUNTIME_TEST_UNSET_DEEPSEEK_KEY".into());
        let local_without_credential = select_candidate(
            &config,
            "language.respond",
            config.intent("language.respond").unwrap(),
            &config.profiles["balanced"],
            &RequestConstraints::default(),
            None,
        )
        .unwrap();
        assert_eq!(local_without_credential.placement, Placement::Local);
        assert_ne!(
            config.deployments[&local_without_credential.deployment_id].provider,
            "deepseek-cloud"
        );

        config
            .providers
            .get_mut("deepseek-cloud")
            .unwrap()
            .requires_api_key = false;
        let cloud = select_candidate(
            &config,
            "language.respond",
            config.intent("language.respond").unwrap(),
            &config.profiles["balanced"],
            &RequestConstraints {
                placement: Some(infer_core::PlacementScope::CloudOnly),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(cloud.deployment_id, "deepseek_v4_flash");

        let local = select_candidate(
            &config,
            "language.respond",
            config.intent("language.respond").unwrap(),
            &config.profiles["balanced"],
            &RequestConstraints {
                placement: Some(infer_core::PlacementScope::LocalOnly),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(local.placement, Placement::Local);
        assert_ne!(
            config.deployments[&local.deployment_id].provider,
            "deepseek-cloud"
        );
    }

    #[test]
    fn codex_subscription_model_group_maps_capability_floor_and_effort_stays_orthogonal() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let config = RuntimeConfig::load(path).unwrap();
        let constraints = RequestConstraints {
            provider_access_class: Some(ProviderAccessClass::Subscription),
            placement: Some(infer_core::PlacementScope::CloudOnly),
            ..RequestConstraints::default()
        };
        let allowed = BTreeSet::from([
            ProviderAccessClass::Standard,
            ProviderAccessClass::Subscription,
        ]);
        let cloud_inputs = BTreeSet::from([Modality::Text]);
        let empty = BTreeSet::new();

        let language = plan_candidates(
            &config,
            "language.respond",
            config.intent("language.respond").unwrap(),
            &config.profiles["balanced"],
            CandidatePlanningContext {
                constraints: &constraints,
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: Some(ReasoningEffort::Low),
                allowed_provider_access_classes: &allowed,
                allowed_cloud_input_modalities: &cloud_inputs,
                routing_grant: None,
                unavailable_providers: &empty,
                unavailable_deployments: &empty,
            },
        );
        let language_deployments = language
            .candidates
            .iter()
            .map(|candidate| candidate.deployment_id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            language_deployments,
            BTreeSet::from([
                "codex_gpt_5_6_luna",
                "codex_gpt_5_6_sol",
                "codex_gpt_5_6_terra",
                "codex_gpt_6_luna",
                "codex_gpt_6_sol",
            ])
        );
        for (deployment, model, ultra) in [
            ("codex_gpt_6_luna", "gpt-6-luna", false),
            ("codex_gpt_6_sol", "gpt-6-sol", true),
        ] {
            let configured = &config.deployments[deployment];
            assert_eq!(config.model_builds[&configured.build].model_id, model);
            assert_eq!(configured.provider, "codex-subscription");
            assert_eq!(
                configured
                    .supported_efforts
                    .contains(&ReasoningEffort::Ultra),
                ultra
            );
        }

        let deep = plan_candidates(
            &config,
            "reasoning.solve",
            config.intent("reasoning.solve").unwrap(),
            &config.profiles["balanced"],
            CandidatePlanningContext {
                constraints: &constraints,
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: Some(ReasoningEffort::Max),
                allowed_provider_access_classes: &allowed,
                allowed_cloud_input_modalities: &cloud_inputs,
                routing_grant: None,
                unavailable_providers: &empty,
                unavailable_deployments: &empty,
            },
        );
        assert_eq!(deep.candidates[0].deployment_id, "codex_gpt_5_6_luna");

        let strongest = plan_candidates(
            &config,
            "reasoning.solve",
            config.intent("reasoning.solve").unwrap(),
            &config.profiles["capability-first"],
            CandidatePlanningContext {
                constraints: &constraints,
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: Some(ReasoningEffort::Low),
                allowed_provider_access_classes: &allowed,
                allowed_cloud_input_modalities: &cloud_inputs,
                routing_grant: None,
                unavailable_providers: &empty,
                unavailable_deployments: &empty,
            },
        );
        assert_eq!(strongest.candidates[0].deployment_id, "codex_gpt_5_6_sol");

        let ultra = plan_candidates(
            &config,
            "reasoning.solve",
            config.intent("reasoning.solve").unwrap(),
            &config.profiles["capability-first"],
            CandidatePlanningContext {
                constraints: &constraints,
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: Some(ReasoningEffort::Ultra),
                allowed_provider_access_classes: &allowed,
                allowed_cloud_input_modalities: &cloud_inputs,
                routing_grant: None,
                unavailable_providers: &empty,
                unavailable_deployments: &empty,
            },
        );
        assert_eq!(ultra.candidates[0].deployment_id, "codex_gpt_5_6_sol");
        assert!(ultra.decision.candidates.iter().any(|candidate| {
            candidate.deployment == "codex_gpt_5_6_luna"
                && candidate
                    .reason_codes
                    .contains(&CandidateReasonCode::ReasoningEffortUnsupported)
        }));
        assert!(
            ultra
                .candidates
                .iter()
                .any(|candidate| { candidate.deployment_id == "codex_gpt_6_sol" })
        );
        assert!(ultra.decision.candidates.iter().any(|candidate| {
            candidate.deployment == "codex_gpt_6_luna"
                && candidate
                    .reason_codes
                    .contains(&CandidateReasonCode::ReasoningEffortUnsupported)
        }));
    }

    #[test]
    fn language_capability_floors_exclude_weaker_models_and_select_exact_tiers() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let config = RuntimeConfig::load(path).unwrap();
        let intent = config.intent("language.respond").unwrap();
        let allowed = BTreeSet::from([
            ProviderAccessClass::Standard,
            ProviderAccessClass::Subscription,
        ]);
        let cloud_inputs = BTreeSet::from([Modality::Text]);
        let empty = BTreeSet::new();

        let plan = |capability_floor| {
            let constraints = RequestConstraints {
                capability_floor: Some(capability_floor),
                max_cost_usd: Some(0.0),
                ..RequestConstraints::default()
            };
            plan_candidates(
                &config,
                "language.respond",
                intent,
                &config.profiles["balanced"],
                CandidatePlanningContext {
                    constraints: &constraints,
                    execution_requirements: &ExecutionRequirements::default(),
                    reasoning_effort: None,
                    allowed_provider_access_classes: &allowed,
                    allowed_cloud_input_modalities: &cloud_inputs,
                    routing_grant: None,
                    unavailable_providers: &empty,
                    unavailable_deployments: &empty,
                },
            )
        };

        let advanced = plan(CapabilityLevel::Advanced);
        assert_eq!(
            advanced.candidates.first().unwrap().deployment_id,
            "codex_gpt_5_6_luna"
        );
        let capable_local = advanced
            .decision
            .candidates
            .iter()
            .find(|candidate| {
                config.deployments[&candidate.deployment].provider == "ollama-local"
                    && candidate
                        .reason_codes
                        .contains(&CandidateReasonCode::CapabilityBelowFloor)
            })
            .unwrap();
        assert_eq!(capable_local.status, CandidateDecisionStatus::Rejected);
        assert_eq!(
            capable_local.reason_codes,
            vec![CandidateReasonCode::CapabilityBelowFloor]
        );

        let expert = plan(CapabilityLevel::Expert);
        assert_eq!(
            expert.candidates.first().unwrap().deployment_id,
            "codex_gpt_5_6_terra"
        );
        let advanced_luna = expert
            .decision
            .candidates
            .iter()
            .find(|candidate| candidate.deployment == "codex_gpt_5_6_luna")
            .unwrap();
        assert_eq!(advanced_luna.status, CandidateDecisionStatus::Rejected);
        assert_eq!(
            advanced_luna.reason_codes,
            vec![CandidateReasonCode::CapabilityBelowFloor]
        );

        let exceptional = plan(CapabilityLevel::Exceptional);
        assert_eq!(
            exceptional.candidates.first().unwrap().deployment_id,
            "codex_gpt_5_6_sol"
        );
        let expert_terra = exceptional
            .decision
            .candidates
            .iter()
            .find(|candidate| candidate.deployment == "codex_gpt_5_6_terra")
            .unwrap();
        assert_eq!(expert_terra.status, CandidateDecisionStatus::Rejected);
        assert_eq!(
            expert_terra.reason_codes,
            vec![CandidateReasonCode::CapabilityBelowFloor]
        );
    }

    #[test]
    fn exceptional_multimodal_routes_to_sol_and_rejects_terra() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let config = RuntimeConfig::load(path).unwrap();
        let intent = config.intent("multimodal.respond").unwrap();
        let constraints = RequestConstraints {
            capability_floor: Some(CapabilityLevel::Exceptional),
            max_cost_usd: Some(0.0),
            ..RequestConstraints::default()
        };
        let allowed = BTreeSet::from([ProviderAccessClass::Subscription]);
        let cloud_inputs = BTreeSet::from([Modality::Text, Modality::Image]);
        let empty = BTreeSet::new();
        let plan = plan_candidates(
            &config,
            "multimodal.respond",
            intent,
            &config.profiles["balanced"],
            CandidatePlanningContext {
                constraints: &constraints,
                execution_requirements: &ExecutionRequirements {
                    input_modalities: BTreeSet::from([Modality::Text, Modality::Image]),
                    ..ExecutionRequirements::default()
                },
                reasoning_effort: None,
                allowed_provider_access_classes: &allowed,
                allowed_cloud_input_modalities: &cloud_inputs,
                routing_grant: None,
                unavailable_providers: &empty,
                unavailable_deployments: &empty,
            },
        );

        assert_eq!(
            plan.candidates.first().unwrap().deployment_id,
            "codex_gpt_5_6_sol"
        );
        let terra = plan
            .decision
            .candidates
            .iter()
            .find(|candidate| candidate.deployment == "codex_gpt_5_6_terra")
            .unwrap();
        assert_eq!(terra.status, CandidateDecisionStatus::Rejected);
        assert_eq!(
            terra.reason_codes,
            vec![CandidateReasonCode::CapabilityBelowFloor]
        );
    }

    #[test]
    fn image_generation_routes_only_to_the_explicit_codex_subscription_slice() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let config = RuntimeConfig::load(path).unwrap();
        let intent = config.intent(infer_core::IMAGE_GENERATION_INTENT).unwrap();
        let requirements = ExecutionRequirements {
            provider_capabilities: BTreeSet::from([
                infer_core::ProviderCapability::Responses,
                infer_core::ProviderCapability::ImageGeneration,
            ]),
            model_features: BTreeSet::from(["image_generation".into()]),
            input_modalities: BTreeSet::from([Modality::Text]),
            execution_mode: infer_core::ExecutionMode::Unary,
        };
        let constraints = RequestConstraints {
            provider_access_class: Some(ProviderAccessClass::Subscription),
            max_cost_usd: Some(0.0),
            fallback: Some(Fallback::None),
            ..RequestConstraints::default()
        };
        let empty = BTreeSet::new();
        let denied = plan_candidates(
            &config,
            infer_core::IMAGE_GENERATION_INTENT,
            intent,
            &config.profiles["balanced"],
            CandidatePlanningContext {
                constraints: &constraints,
                execution_requirements: &requirements,
                reasoning_effort: None,
                allowed_provider_access_classes: &BTreeSet::from([ProviderAccessClass::Standard]),
                allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text]),
                routing_grant: None,
                unavailable_providers: &empty,
                unavailable_deployments: &empty,
            },
        );
        assert!(denied.candidates.is_empty());
        assert!(denied.decision.candidates.iter().any(|candidate| {
            candidate.deployment == "codex_gpt_5_6_luna"
                && candidate
                    .reason_codes
                    .contains(&CandidateReasonCode::ProviderAccessNotAllowed)
        }));

        let admitted = plan_candidates(
            &config,
            infer_core::IMAGE_GENERATION_INTENT,
            intent,
            &config.profiles["balanced"],
            CandidatePlanningContext {
                constraints: &constraints,
                execution_requirements: &requirements,
                reasoning_effort: None,
                allowed_provider_access_classes: &BTreeSet::from([
                    ProviderAccessClass::Standard,
                    ProviderAccessClass::Subscription,
                ]),
                allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text]),
                routing_grant: None,
                unavailable_providers: &empty,
                unavailable_deployments: &empty,
            },
        );
        assert_eq!(admitted.candidates.len(), 1);
        assert_eq!(admitted.candidates[0].deployment_id, "codex_gpt_5_6_luna");
    }

    #[test]
    fn resource_cost_prefers_the_light_model_after_capability_floor_is_met() {
        let config: RuntimeConfig =
            toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
        config.validate().unwrap();
        let intent = config.intent("text.summarize").unwrap();
        let selected = select_candidate(
            &config,
            "text.summarize",
            intent,
            &config.profiles["cost-first"],
            &RequestConstraints::default(),
            None,
        )
        .unwrap();
        assert_eq!(selected.deployment_id, "ollama_qwen3_5_2b");
        assert_eq!(selected.resource_class, infer_core::ResourceClass::Light);
    }

    #[test]
    fn bounded_text_deduplication_selects_the_foundational_local_build() {
        let config: RuntimeConfig =
            toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
        config.validate().unwrap();
        let intent = config.intent("text.deduplicate").unwrap();
        let selected = select_candidate(
            &config,
            "text.deduplicate",
            intent,
            &config.profiles[intent.default_policy.as_deref().unwrap()],
            &RequestConstraints::default(),
            None,
        )
        .unwrap();
        assert_eq!(selected.deployment_id, "ollama_qwen3_5_4b");
        assert_eq!(selected.capability_level, CapabilityLevel::Foundational);
        assert_eq!(selected.resource_class, infer_core::ResourceClass::Standard);
    }

    #[test]
    fn named_route_is_ordered_and_never_escapes_the_effective_intent_grant() {
        let config: RuntimeConfig =
            toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
        config.validate().unwrap();
        let intent = config.intent("text.summarize").unwrap();
        let constraints = RequestConstraints {
            named_route: Some(NamedRouteRequest::Deployments(vec![
                "ollama_qwen3_5_4b".into(),
                "ollama_qwen3_5_2b".into(),
            ])),
            fallback: Some(Fallback::Equivalent),
            ..Default::default()
        };
        let grant = RoutingGrantConfig {
            deployment_ids: BTreeSet::from([
                "ollama_qwen3_5_2b".into(),
                "ollama_qwen3_5_4b".into(),
            ]),
            model_profile_ids: BTreeSet::new(),
            ..Default::default()
        };
        let empty = BTreeSet::new();
        let plan = plan_candidates(
            &config,
            "text.summarize",
            intent,
            &config.profiles["cost-first"],
            CandidatePlanningContext {
                constraints: &constraints,
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: None,
                allowed_provider_access_classes: &BTreeSet::from([ProviderAccessClass::Standard]),
                allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text]),
                routing_grant: Some(&grant),
                unavailable_providers: &empty,
                unavailable_deployments: &empty,
            },
        );
        assert_eq!(plan.candidates[0].deployment_id, "ollama_qwen3_5_4b");
        assert_eq!(plan.candidates[1].deployment_id, "ollama_qwen3_5_2b");
        assert!(plan.decision.candidates.iter().all(|candidate| {
            candidate.status == CandidateDecisionStatus::Rejected
                || matches!(
                    candidate.deployment.as_str(),
                    "ollama_qwen3_5_4b" | "ollama_qwen3_5_2b"
                )
        }));
    }

    #[test]
    fn named_only_grant_is_excluded_by_default_but_admitted_when_explicitly_requested() {
        let config: RuntimeConfig =
            toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
        config.validate().unwrap();
        let intent = config.intent("text.summarize").unwrap();
        let empty = BTreeSet::new();
        let grant = RoutingGrantConfig {
            deployment_ids: BTreeSet::from(["ollama_qwen3_5_4b".into()]),
            named_deployment_ids: BTreeSet::from(["ollama_qwen3_5_2b".into()]),
            ..Default::default()
        };

        let ordinary = plan_candidates(
            &config,
            "text.summarize",
            intent,
            &config.profiles["cost-first"],
            CandidatePlanningContext {
                constraints: &RequestConstraints::default(),
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: None,
                allowed_provider_access_classes: &BTreeSet::from([ProviderAccessClass::Standard]),
                allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text]),
                routing_grant: Some(&grant),
                unavailable_providers: &empty,
                unavailable_deployments: &empty,
            },
        );
        assert_eq!(ordinary.candidates.len(), 1);
        assert_eq!(ordinary.candidates[0].deployment_id, "ollama_qwen3_5_4b");

        let requested = RequestConstraints {
            named_route: Some(NamedRouteRequest::Deployments(vec![
                "ollama_qwen3_5_2b".into(),
            ])),
            fallback: Some(Fallback::None),
            ..Default::default()
        };
        assert!(grant.allows_request(requested.named_route.as_ref().unwrap(), &config));
        let named = plan_candidates(
            &config,
            "text.summarize",
            intent,
            &config.profiles["cost-first"],
            CandidatePlanningContext {
                constraints: &requested,
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: None,
                allowed_provider_access_classes: &BTreeSet::from([ProviderAccessClass::Standard]),
                allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text]),
                routing_grant: Some(&grant),
                unavailable_providers: &empty,
                unavailable_deployments: &empty,
            },
        );
        assert_eq!(named.candidates.len(), 1);
        assert_eq!(named.candidates[0].deployment_id, "ollama_qwen3_5_2b");
    }

    #[test]
    fn empty_intent_grant_denies_candidates_without_using_routing_default() {
        let config: RuntimeConfig =
            toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
        let intent = config.intent("text.summarize").unwrap();
        let empty = BTreeSet::new();
        let grant = RoutingGrantConfig::default();
        let plan = plan_candidates(
            &config,
            "text.summarize",
            intent,
            &config.profiles["balanced"],
            CandidatePlanningContext {
                constraints: &RequestConstraints::default(),
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: None,
                allowed_provider_access_classes: &BTreeSet::from([ProviderAccessClass::Standard]),
                allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text]),
                routing_grant: Some(&grant),
                unavailable_providers: &empty,
                unavailable_deployments: &empty,
            },
        );
        assert!(plan.candidates.is_empty());
        for candidate in &plan.decision.candidates {
            let deployment = &config.deployments[&candidate.deployment];
            let profile = &config.model_builds[&deployment.build].profile;
            if config.model_profiles[profile]
                .ratings
                .contains_key("text.summarize")
            {
                assert!(
                    candidate
                        .reason_codes
                        .contains(&CandidateReasonCode::RoutingGrantExcluded)
                );
            }
        }
    }

    #[test]
    fn unavailable_primary_does_not_enable_an_authorized_backup_when_fallback_is_none() {
        let config: RuntimeConfig =
            toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
        let intent = config.intent("text.summarize").unwrap();
        let constraints = RequestConstraints {
            named_route: Some(NamedRouteRequest::Deployments(vec![
                "ollama_qwen3_5_4b".into(),
                "ollama_qwen3_5_2b".into(),
            ])),
            fallback: Some(Fallback::None),
            ..Default::default()
        };
        let grant = RoutingGrantConfig {
            deployment_ids: BTreeSet::from([
                "ollama_qwen3_5_4b".into(),
                "ollama_qwen3_5_2b".into(),
            ]),
            model_profile_ids: BTreeSet::new(),
            ..Default::default()
        };
        let empty = BTreeSet::new();
        let unavailable = BTreeSet::from(["ollama_qwen3_5_4b".into()]);
        let plan = plan_candidates(
            &config,
            "text.summarize",
            intent,
            &config.profiles["local-first"],
            CandidatePlanningContext {
                constraints: &constraints,
                execution_requirements: &ExecutionRequirements::default(),
                reasoning_effort: None,
                allowed_provider_access_classes: &BTreeSet::from([ProviderAccessClass::Standard]),
                allowed_cloud_input_modalities: &BTreeSet::from([Modality::Text]),
                routing_grant: Some(&grant),
                unavailable_providers: &empty,
                unavailable_deployments: &unavailable,
            },
        );
        assert!(plan.candidates.is_empty());
        let decision = plan
            .decision
            .candidates
            .iter()
            .find(|candidate| candidate.deployment == "ollama_qwen3_5_4b")
            .unwrap();
        assert_eq!(decision.status, CandidateDecisionStatus::Rejected);
        assert!(
            decision
                .reason_codes
                .contains(&CandidateReasonCode::DeploymentUnavailable)
        );
        let backup = plan
            .decision
            .candidates
            .iter()
            .find(|candidate| candidate.deployment == "ollama_qwen3_5_2b")
            .unwrap();
        assert!(
            backup
                .reason_codes
                .contains(&CandidateReasonCode::NamedRouteFallbackDisabled)
        );
    }

    #[test]
    fn every_routable_local_audio_intent_resolves_to_its_specialized_build() {
        let config: RuntimeConfig =
            toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
        config.validate().unwrap();
        let expected = [
            ("audio.transcribe", "mlx_qwen3_asr_1_7b"),
            ("audio.align", "mlx_qwen3_forced_aligner_0_6b"),
            ("speech.synthesize", "mlx_qwen3_tts_custom_voice_1_7b"),
            ("speech.design_voice", "mlx_qwen3_tts_voice_design_1_7b"),
            ("speech.clone_voice", "mlx_qwen3_tts_base_1_7b"),
        ];
        for (intent_id, deployment_id) in expected {
            let intent = config.intent(intent_id).unwrap();
            let profile = &config.profiles[intent.default_policy.as_deref().unwrap()];
            let selected = select_candidate(
                &config,
                intent_id,
                intent,
                profile,
                &RequestConstraints {
                    placement: Some(infer_core::PlacementScope::LocalOnly),
                    offline_required: Some(true),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            assert_eq!(selected.deployment_id, deployment_id);
            assert_eq!(selected.placement, Placement::Local);
        }
    }
}

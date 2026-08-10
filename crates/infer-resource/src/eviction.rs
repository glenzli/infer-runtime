//! Deterministic pressure-to-eviction recommendation policy.
//!
//! This module owns policy evaluation and candidate selection only. It cannot
//! call a native provider or mutate lifecycle state; `lifecycle` remains the
//! sole owner of reservations and load/unload transitions.

use std::collections::BTreeSet;

use infer_core::{EvictionMode, EvictionPolicyConfig, ResourceClass};
use serde::Serialize;

use super::{SystemPressureLevel, SystemPressureSnapshot, lifecycle::ModelLifecycleState};

/// Policy inputs after static config has been normalized for one evaluation.
#[derive(Debug, Clone, Copy)]
pub struct EvictionPolicy {
    pub max_benchmark_age_ms: u64,
}

/// Fully resolved safety values for one deployment. Resolution is kept in the
/// policy owner and performed once before candidate evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ResolvedEvictionSafety {
    pub automatic_eligible: bool,
    pub minimum_resident_ms: u64,
}

#[derive(Debug, Clone)]
pub struct EvictionRequest {
    pub bytes_to_free: u64,
    /// Deployments needed by already-admitted or queued higher-priority work.
    /// They cannot become eviction candidates even with no active attempt.
    pub protected_deployments: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct EvictionModel {
    pub deployment: String,
    pub resource_class: ResourceClass,
    pub safety: ResolvedEvictionSafety,
    pub state: ModelLifecycleState,
    pub resident_memory_bytes: Option<u64>,
    pub active_reservations: usize,
    pub state_since_unix_ms: Option<u64>,
    pub last_used_unix_ms: Option<u64>,
    /// A benchmarked estimate. Absence is intentionally a hard stop: runtime
    /// must not evict a model when its reload cost is unknown.
    pub reload_cost_ms: Option<u64>,
    /// The benchmark timestamp is evaluated against the configured maximum
    /// age, preventing an old measurement from silently becoming policy.
    pub benchmark_observed_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvictionSkipReason {
    NotReady,
    ActiveReservation,
    PendingDemand,
    AutomaticEvictionDisabled,
    MinimumResidency,
    MissingResidentSize,
    MissingReloadEstimate,
    StaleReloadEstimate,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvictionSkip {
    pub deployment: String,
    pub reason: EvictionSkipReason,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvictionTarget {
    pub deployment: String,
    pub resource_class: ResourceClass,
    pub minimum_resident_ms: u64,
    pub resident_memory_bytes: u64,
    pub reload_cost_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvictionPlan {
    pub requested_bytes: u64,
    pub projected_freed_bytes: u64,
    pub shortfall_bytes: u64,
    pub targets: Vec<EvictionTarget>,
    pub skipped: Vec<EvictionSkip>,
}

/// Non-mutating resource policy projection. The distinction between no
/// trigger, insufficient host data, and an empty conservative plan is useful
/// to operators and keeps a future action owner from guessing intent.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EvictionRecommendation {
    Disabled,
    NoPressureTrigger {
        pressure: SystemPressureLevel,
    },
    NoTargetConfigured {
        pressure: SystemPressureLevel,
    },
    InsufficientPressureData {
        pressure: SystemPressureLevel,
    },
    TargetAlreadyMet {
        pressure: SystemPressureLevel,
        target_free_memory_percent: u8,
    },
    Planned {
        pressure: SystemPressureLevel,
        target_free_memory_percent: u8,
        current_free_memory_bytes: u64,
        target_free_memory_bytes: u64,
        plan: EvictionPlan,
    },
}

/// Convert a host pressure observation and versioned policy inputs into a
/// deterministic, dry-run eviction plan. This function never initiates a
/// lifecycle action, including when the resulting plan has no shortfall.
pub fn recommend_eviction(
    now_unix_ms: u64,
    config: &EvictionPolicyConfig,
    pressure: &SystemPressureSnapshot,
    protected_deployments: BTreeSet<String>,
    models: impl IntoIterator<Item = EvictionModel>,
) -> EvictionRecommendation {
    if config.mode == EvictionMode::Disabled {
        return EvictionRecommendation::Disabled;
    }
    let target_free_memory_percent = match pressure.level {
        SystemPressureLevel::Elevated => config.elevated.target_free_memory_percent,
        SystemPressureLevel::Critical => config.critical.target_free_memory_percent,
        SystemPressureLevel::Unknown | SystemPressureLevel::Normal => {
            return EvictionRecommendation::NoPressureTrigger {
                pressure: pressure.level,
            };
        }
    };
    let Some(target_free_memory_percent) = target_free_memory_percent else {
        return EvictionRecommendation::NoTargetConfigured {
            pressure: pressure.level,
        };
    };
    let (Some(total_memory_bytes), Some(free_memory_percent)) =
        (pressure.total_memory_bytes, pressure.free_memory_percent)
    else {
        return EvictionRecommendation::InsufficientPressureData {
            pressure: pressure.level,
        };
    };
    let current_free_memory_bytes = percent_of(total_memory_bytes, free_memory_percent);
    let target_free_memory_bytes = percent_of(total_memory_bytes, target_free_memory_percent);
    if current_free_memory_bytes >= target_free_memory_bytes {
        return EvictionRecommendation::TargetAlreadyMet {
            pressure: pressure.level,
            target_free_memory_percent,
        };
    }
    let plan = plan_eviction(
        now_unix_ms,
        EvictionPolicy {
            max_benchmark_age_ms: config.max_benchmark_age_ms,
        },
        &EvictionRequest {
            bytes_to_free: target_free_memory_bytes.saturating_sub(current_free_memory_bytes),
            protected_deployments,
        },
        models,
    );
    EvictionRecommendation::Planned {
        pressure: pressure.level,
        target_free_memory_percent,
        current_free_memory_bytes,
        target_free_memory_bytes,
        plan,
    }
}

/// Compute a deterministic, conservative eviction plan. It never mutates a
/// lifecycle state. A separate native action owner must enact every target and
/// reconcile the result before considering more work.
pub fn plan_eviction(
    now_unix_ms: u64,
    policy: EvictionPolicy,
    request: &EvictionRequest,
    models: impl IntoIterator<Item = EvictionModel>,
) -> EvictionPlan {
    let mut candidates = Vec::new();
    let mut skipped = Vec::new();
    for model in models {
        let reason = if model.state != ModelLifecycleState::Ready {
            Some(EvictionSkipReason::NotReady)
        } else if model.active_reservations != 0 {
            Some(EvictionSkipReason::ActiveReservation)
        } else if request.protected_deployments.contains(&model.deployment) {
            Some(EvictionSkipReason::PendingDemand)
        } else if !model.safety.automatic_eligible {
            Some(EvictionSkipReason::AutomaticEvictionDisabled)
        } else if model.state_since_unix_ms.is_none_or(|ready_at| {
            now_unix_ms.saturating_sub(ready_at) < model.safety.minimum_resident_ms
        }) {
            Some(EvictionSkipReason::MinimumResidency)
        } else if model.resident_memory_bytes.is_none_or(|bytes| bytes == 0) {
            Some(EvictionSkipReason::MissingResidentSize)
        } else if model.reload_cost_ms.is_none() {
            Some(EvictionSkipReason::MissingReloadEstimate)
        } else if model
            .benchmark_observed_at_unix_ms
            .is_none_or(|observed_at| {
                now_unix_ms.saturating_sub(observed_at) > policy.max_benchmark_age_ms
            })
        {
            Some(EvictionSkipReason::StaleReloadEstimate)
        } else {
            None
        };
        if let Some(reason) = reason {
            skipped.push(EvictionSkip {
                deployment: model.deployment,
                reason,
            });
        } else {
            candidates.push(model);
        }
    }
    candidates.sort_by(|left, right| {
        left.last_used_unix_ms
            .unwrap_or_default()
            .cmp(&right.last_used_unix_ms.unwrap_or_default())
            .then_with(|| left.reload_cost_ms.cmp(&right.reload_cost_ms))
            .then_with(|| right.resident_memory_bytes.cmp(&left.resident_memory_bytes))
            .then_with(|| left.deployment.cmp(&right.deployment))
    });
    let mut projected_freed_bytes = 0;
    let mut targets = Vec::new();
    for candidate in candidates {
        if projected_freed_bytes >= request.bytes_to_free {
            break;
        }
        let resident_memory_bytes = candidate.resident_memory_bytes.expect("filtered above");
        let reload_cost_ms = candidate.reload_cost_ms.expect("filtered above");
        projected_freed_bytes = projected_freed_bytes.saturating_add(resident_memory_bytes);
        targets.push(EvictionTarget {
            deployment: candidate.deployment,
            resource_class: candidate.resource_class,
            minimum_resident_ms: candidate.safety.minimum_resident_ms,
            resident_memory_bytes,
            reload_cost_ms,
        });
    }
    EvictionPlan {
        requested_bytes: request.bytes_to_free,
        projected_freed_bytes,
        shortfall_bytes: request.bytes_to_free.saturating_sub(projected_freed_bytes),
        targets,
        skipped,
    }
}

/// Resolve global, resource-class and deployment safety in increasing order
/// of specificity. Sparse overrides make inheritance explicit and prevent a
/// timing-only override from changing eligibility.
pub fn resolve_eviction_safety(
    config: &EvictionPolicyConfig,
    deployment: &str,
    resource_class: ResourceClass,
) -> ResolvedEvictionSafety {
    let mut resolved = ResolvedEvictionSafety {
        automatic_eligible: config.automatic_eligible,
        minimum_resident_ms: config.minimum_resident_ms,
    };
    if let Some(class) = config.classes.get(&resource_class) {
        apply_safety_override(&mut resolved, class);
    }
    if let Some(deployment) = config.deployments.get(deployment) {
        apply_safety_override(&mut resolved, deployment);
    }
    resolved
}

fn apply_safety_override(
    resolved: &mut ResolvedEvictionSafety,
    override_config: &infer_core::EvictionSafetyOverrideConfig,
) {
    if let Some(automatic_eligible) = override_config.automatic_eligible {
        resolved.automatic_eligible = automatic_eligible;
    }
    if let Some(minimum_resident_ms) = override_config.minimum_resident_ms {
        resolved.minimum_resident_ms = minimum_resident_ms;
    }
}

fn percent_of(total: u64, percent: u8) -> u64 {
    ((u128::from(total) * u128::from(percent)) / 100)
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(deployment: &str) -> EvictionModel {
        EvictionModel {
            deployment: deployment.into(),
            resource_class: ResourceClass::Standard,
            safety: ResolvedEvictionSafety {
                automatic_eligible: true,
                minimum_resident_ms: 0,
            },
            state: ModelLifecycleState::Ready,
            resident_memory_bytes: Some(100),
            active_reservations: 0,
            state_since_unix_ms: Some(10),
            last_used_unix_ms: Some(20),
            reload_cost_ms: Some(1_000),
            benchmark_observed_at_unix_ms: Some(900),
        }
    }

    fn recommend_policy() -> EvictionPolicyConfig {
        EvictionPolicyConfig {
            mode: EvictionMode::Recommend,
            automatic_eligible: true,
            minimum_resident_ms: 0,
            max_benchmark_age_ms: 1_000,
            classes: Default::default(),
            deployments: Default::default(),
            monitor: Default::default(),
            elevated: infer_core::PressureTargetConfig {
                target_free_memory_percent: Some(20),
            },
            critical: infer_core::PressureTargetConfig {
                target_free_memory_percent: Some(25),
            },
        }
    }

    #[test]
    fn active_reservation_can_never_be_an_eviction_target() {
        let mut protected = model("protected");
        protected.active_reservations = 1;
        let plan = plan_eviction(
            1_000,
            EvictionPolicy {
                max_benchmark_age_ms: 1_000,
            },
            &EvictionRequest {
                bytes_to_free: 100,
                protected_deployments: BTreeSet::new(),
            },
            [protected, model("idle")],
        );
        assert_eq!(plan.targets[0].deployment, "idle");
        assert!(plan.skipped.iter().any(|skip| {
            skip.deployment == "protected" && skip.reason == EvictionSkipReason::ActiveReservation
        }));
    }

    #[test]
    fn anti_thrashing_missing_and_stale_benchmarks_fail_closed() {
        let mut warming = model("warming");
        warming.state_since_unix_ms = Some(950);
        warming.safety.minimum_resident_ms = 100;
        let mut unknown_cost = model("unknown_cost");
        unknown_cost.reload_cost_ms = None;
        let mut stale = model("stale");
        stale.benchmark_observed_at_unix_ms = Some(100);
        let plan = plan_eviction(
            1_000,
            EvictionPolicy {
                max_benchmark_age_ms: 500,
            },
            &EvictionRequest {
                bytes_to_free: 300,
                protected_deployments: BTreeSet::from(["pending".into()]),
            },
            [warming, unknown_cost, stale, model("pending")],
        );
        assert!(plan.targets.is_empty());
        assert_eq!(plan.shortfall_bytes, 300);
        for reason in [
            EvictionSkipReason::MinimumResidency,
            EvictionSkipReason::MissingReloadEstimate,
            EvictionSkipReason::StaleReloadEstimate,
            EvictionSkipReason::PendingDemand,
        ] {
            assert!(plan.skipped.iter().any(|skip| skip.reason == reason));
        }
    }

    #[test]
    fn plan_is_deterministic_and_prefers_oldest_low_cost_model() {
        let mut older = model("older");
        older.last_used_unix_ms = Some(10);
        older.reload_cost_ms = Some(50);
        let mut newer = model("newer");
        newer.last_used_unix_ms = Some(20);
        newer.reload_cost_ms = Some(1);
        let request = EvictionRequest {
            bytes_to_free: 100,
            protected_deployments: BTreeSet::new(),
        };
        let policy = EvictionPolicy {
            max_benchmark_age_ms: 1_000,
        };
        let first = plan_eviction(1_000, policy, &request, [newer.clone(), older.clone()]);
        let second = plan_eviction(1_000, policy, &request, [older, newer]);
        assert_eq!(first.targets[0].deployment, "older");
        assert_eq!(
            first
                .targets
                .iter()
                .map(|target| &target.deployment)
                .collect::<Vec<_>>(),
            second
                .targets
                .iter()
                .map(|target| &target.deployment)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn pressure_recommendation_calculates_a_dry_run_target() {
        let pressure = SystemPressureSnapshot {
            source: "test".into(),
            level: SystemPressureLevel::Critical,
            last_checked_unix_ms: 1_000,
            total_memory_bytes: Some(1_000),
            free_memory_percent: Some(5),
            last_error: None,
        };
        let recommendation = recommend_eviction(
            1_000,
            &recommend_policy(),
            &pressure,
            BTreeSet::new(),
            [model("first"), model("second")],
        );
        let EvictionRecommendation::Planned {
            target_free_memory_bytes,
            plan,
            ..
        } = recommendation
        else {
            panic!("critical pressure should produce a recommendation");
        };
        assert_eq!(target_free_memory_bytes, 250);
        assert_eq!(plan.requested_bytes, 200);
        assert_eq!(plan.projected_freed_bytes, 200);
    }

    #[test]
    fn disabled_or_unmeasurable_policy_never_invents_an_action() {
        let pressure = SystemPressureSnapshot {
            source: "test".into(),
            level: SystemPressureLevel::Critical,
            last_checked_unix_ms: 1_000,
            total_memory_bytes: None,
            free_memory_percent: None,
            last_error: None,
        };
        assert!(matches!(
            recommend_eviction(
                1_000,
                &EvictionPolicyConfig::default(),
                &pressure,
                BTreeSet::new(),
                [model("local")]
            ),
            EvictionRecommendation::Disabled
        ));
        assert!(matches!(
            recommend_eviction(
                1_000,
                &recommend_policy(),
                &pressure,
                BTreeSet::new(),
                [model("local")]
            ),
            EvictionRecommendation::InsufficientPressureData { .. }
        ));
    }

    #[test]
    fn safety_resolution_is_global_then_class_then_deployment() {
        let mut config = recommend_policy();
        config.automatic_eligible = false;
        config.minimum_resident_ms = 300_000;
        config.classes.insert(
            ResourceClass::Heavy,
            infer_core::EvictionSafetyOverrideConfig {
                automatic_eligible: Some(false),
                minimum_resident_ms: Some(3_600_000),
            },
        );
        config.deployments.insert(
            "measured_vl".into(),
            infer_core::EvictionSafetyOverrideConfig {
                automatic_eligible: Some(true),
                minimum_resident_ms: Some(1_200_000),
            },
        );

        assert_eq!(
            resolve_eviction_safety(&config, "unmeasured_35b", ResourceClass::Heavy),
            ResolvedEvictionSafety {
                automatic_eligible: false,
                minimum_resident_ms: 3_600_000,
            }
        );
        assert_eq!(
            resolve_eviction_safety(&config, "measured_vl", ResourceClass::Heavy),
            ResolvedEvictionSafety {
                automatic_eligible: true,
                minimum_resident_ms: 1_200_000,
            }
        );
    }

    #[test]
    fn critical_pressure_simulation_uses_real_reload_profiles_and_protects_35b() {
        const GIB: u64 = 1_073_741_824;
        let mut policy = recommend_policy();
        policy.max_benchmark_age_ms = 30 * 24 * 60 * 60 * 1_000;
        policy.automatic_eligible = false;
        policy.classes.insert(
            ResourceClass::Light,
            infer_core::EvictionSafetyOverrideConfig {
                automatic_eligible: Some(true),
                minimum_resident_ms: Some(300_000),
            },
        );
        policy.classes.insert(
            ResourceClass::Standard,
            infer_core::EvictionSafetyOverrideConfig {
                automatic_eligible: Some(true),
                minimum_resident_ms: Some(600_000),
            },
        );
        policy.classes.insert(
            ResourceClass::Heavy,
            infer_core::EvictionSafetyOverrideConfig {
                automatic_eligible: Some(false),
                minimum_resident_ms: Some(3_600_000),
            },
        );
        policy.deployments.insert(
            "ollama_qwen3_vl_8b".into(),
            infer_core::EvictionSafetyOverrideConfig {
                automatic_eligible: Some(true),
                minimum_resident_ms: Some(1_200_000),
            },
        );
        let now = 10_000_000;
        let scenario_model = |deployment: &str,
                              resource_class: ResourceClass,
                              resident_gib: u64,
                              reload_cost_ms: Option<u64>| {
            let mut candidate = model(deployment);
            candidate.resource_class = resource_class;
            candidate.safety = resolve_eviction_safety(&policy, deployment, resource_class);
            candidate.resident_memory_bytes = Some(resident_gib * GIB);
            candidate.state_since_unix_ms = Some(1_000_000);
            candidate.last_used_unix_ms = Some(2_000_000);
            candidate.reload_cost_ms = reload_cost_ms;
            candidate.benchmark_observed_at_unix_ms = reload_cost_ms.map(|_| 9_000_000);
            candidate
        };
        let pressure = SystemPressureSnapshot {
            source: "simulation".into(),
            level: SystemPressureLevel::Critical,
            last_checked_unix_ms: now,
            total_memory_bytes: Some(32 * GIB),
            free_memory_percent: Some(5),
            last_error: None,
        };
        let recommendation = recommend_eviction(
            now,
            &policy,
            &pressure,
            BTreeSet::new(),
            [
                scenario_model("ollama_qwen3_5_2b", ResourceClass::Light, 3, Some(636)),
                scenario_model("ollama_qwen3_5_4b", ResourceClass::Standard, 4, Some(649)),
                scenario_model(
                    "ollama_qwen3_vl_4b",
                    ResourceClass::Standard,
                    4,
                    Some(2_346),
                ),
                scenario_model("ollama_qwen3_vl_8b", ResourceClass::Heavy, 7, Some(2_572)),
                scenario_model("ollama_qwen3_6_35b", ResourceClass::Heavy, 21, None),
            ],
        );
        let EvictionRecommendation::Planned { plan, .. } = recommendation else {
            panic!("critical scenario should produce a dry-run plan");
        };
        assert_eq!(
            plan.targets
                .iter()
                .map(|target| target.deployment.as_str())
                .collect::<Vec<_>>(),
            ["ollama_qwen3_5_2b", "ollama_qwen3_5_4b"]
        );
        assert_eq!(plan.requested_bytes, 6_871_947_674);
        assert_eq!(plan.projected_freed_bytes, 7 * GIB);
        assert_eq!(plan.shortfall_bytes, 0);
        assert!(plan.skipped.iter().any(|skip| {
            skip.deployment == "ollama_qwen3_6_35b"
                && skip.reason == EvictionSkipReason::AutomaticEvictionDisabled
        }));
    }
}

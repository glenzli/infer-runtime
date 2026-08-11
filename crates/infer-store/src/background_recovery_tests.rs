use std::collections::BTreeSet;

use infer_core::{
    AttemptOutcome, AttemptSnapshot, AttemptTrigger, CandidateDecision, CandidateDecisionStatus,
    CapabilityLevel, DurablePayloadKind, DurablePayloadRef, EvaluationStatus, JobSnapshot,
    JobState, Placement, Priority, RequestConstraints, ResourceClass, RoutingDecision,
};
use serde_json::json;

use crate::{AttemptReservation, AuditEventInput, ConfigSnapshot, Store};

const JOBS: usize = 512;

fn snapshot(index: usize) -> JobSnapshot {
    JobSnapshot {
        id: format!("resp_soak_{index:08x}"),
        app_id: "test-app".into(),
        intent: "text.summarize".into(),
        provider: "local".into(),
        deployment: "small".into(),
        model_profile: "qwen".into(),
        model_build: "qwen_small".into(),
        physical_model: "qwen:2b".into(),
        placement: Placement::Local,
        capability_level: CapabilityLevel::Foundational,
        evaluation_status: EvaluationStatus::Benchmarked,
        resource_class: ResourceClass::Light,
        state: JobState::Running,
        policy: "balanced".into(),
        priority: Priority::Background,
        constraints: RequestConstraints::default(),
        routing: RoutingDecision {
            capability_floor: CapabilityLevel::Foundational,
            candidates: vec![CandidateDecision {
                deployment: "small".into(),
                provider: "local".into(),
                status: CandidateDecisionStatus::Eligible,
                rank: Some(1),
                reason_codes: Vec::new(),
            }],
        },
        attempts: vec![AttemptSnapshot {
            number: 1,
            provider: "local".into(),
            deployment: "small".into(),
            outcome: AttemptOutcome::Running,
            trigger: AttemptTrigger::Initial,
            error_kind: None,
            error: None,
        }],
        error: None,
    }
}

fn payload(index: usize) -> DurablePayloadRef {
    DurablePayloadRef {
        blob_id: format!("pay_{index:032x}"),
        kind: DurablePayloadKind::ResponsesRequest,
        digest: format!("hmac-sha256:{index:064x}"),
        plaintext_bytes: 128,
    }
}

#[test]
fn repeated_restart_soak_converges_at_the_replay_limit_without_leaks() {
    let store = Store::open_in_memory(
        ConfigSnapshot::from_serializable(&json!({"profile": "background-soak-v1"})).unwrap(),
    )
    .unwrap();
    for index in 0..JOBS {
        let snapshot = snapshot(index);
        store
            .persist_background_job_with_event(
                &snapshot,
                &payload(index),
                AuditEventInput {
                    kind: "job.admitted".into(),
                    details: json!({"soak": true}),
                },
            )
            .unwrap();
        store
            .reserve_attempt(&AttemptReservation {
                job_id: snapshot.id,
                attempt_number: 1,
                app_id: "test-app".into(),
                provider: "local".into(),
                deployment: "small".into(),
                amount_usd: 0.0,
                estimated_tokens: 32,
            })
            .unwrap();
    }

    let first_restart = store.recover_background_jobs(Some(2)).unwrap();
    assert_eq!(first_restart.runnable.len(), JOBS);
    assert!(first_restart.discard.is_empty());
    assert!(first_restart.runnable.iter().all(|job| {
        job.recovery_replays == 1
            && job.snapshot.state == JobState::Queued
            && job.snapshot.attempts[0].outcome == AttemptOutcome::Interrupted
    }));
    assert_eq!(store.usage_entries().unwrap().len(), JOBS);
    assert!(store.active_reservations().unwrap().is_empty());

    let second_restart = store.recover_background_jobs(Some(2)).unwrap();
    assert_eq!(second_restart.runnable.len(), JOBS);
    assert!(second_restart.discard.is_empty());
    assert!(
        second_restart
            .runnable
            .iter()
            .all(|job| job.recovery_replays == 2)
    );

    let exhausted_restart = store.recover_background_jobs(Some(2)).unwrap();
    assert!(exhausted_restart.runnable.is_empty());
    assert_eq!(exhausted_restart.discard.len(), JOBS);
    assert_eq!(store.pending_background_jobs().unwrap(), 0);
    assert!(store.background_payload_refs().unwrap().is_empty());
    assert_eq!(
        exhausted_restart
            .discard
            .iter()
            .map(|reference| reference.blob_id.clone())
            .collect::<BTreeSet<_>>()
            .len(),
        JOBS
    );
    assert_eq!(store.usage_entries().unwrap().len(), JOBS);
    assert!(store.active_reservations().unwrap().is_empty());

    for index in 0..JOBS {
        let snapshot = store
            .load_job(&format!("resp_soak_{index:08x}"))
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.state, JobState::Failed);
        assert_eq!(snapshot.attempts[0].outcome, AttemptOutcome::Interrupted);
    }
    let events = store.audit_events("resp_soak_00000000").unwrap();
    assert_eq!(
        events
            .iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<_>>(),
        vec![
            "job.admitted",
            "background.requeued_after_restart",
            "background.requeued_after_restart",
            "background.recovery_rejected",
        ]
    );
}

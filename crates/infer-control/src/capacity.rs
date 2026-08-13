//! Explicit node-wide, priority-aware admission capacity for local Providers.
//!
//! Provider schedulers own their individual slots. This owner is deliberately
//! narrower: it arbitrates only measured node-wide dimensions which cannot be
//! inferred safely from a Provider (CPU slots, unified memory and accelerator
//! slots). Omitted limits deliberately do nothing.

use std::{collections::VecDeque, time::Duration};

use infer_core::{AdmissionCapacityConfig, DeploymentResourceEstimateConfig, Priority};
use serde::Serialize;
use thiserror::Error;
use tokio::{
    sync::{mpsc, oneshot},
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum NodeCapacityError {
    #[error("node capacity reservation was cancelled")]
    Cancelled,
    #[error("node capacity reservation expired before it was admitted")]
    DeadlineExpired,
    #[error("node capacity queue is full")]
    QueueFull,
    #[error("node capacity scheduler is unavailable")]
    Unavailable,
    #[error("deployment claim for `{dimension}` exceeds configured node capacity")]
    ClaimExceedsLimit { dimension: &'static str },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CapacityDimensionSnapshot {
    pub limit: u32,
    pub reserved: u32,
    pub available: u32,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct NodeCapacitySnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_slots: Option<CapacityDimensionSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unified_memory_mib: Option<CapacityDimensionSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accelerator_slots: Option<CapacityDimensionSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<usize>,
}

/// A release guard for one priority-aware node reservation.
#[derive(Debug)]
pub struct NodeCapacityReservation {
    command_tx: Option<mpsc::UnboundedSender<Command>>,
    claim: CapacityClaim,
}

impl Drop for NodeCapacityReservation {
    fn drop(&mut self) {
        if let Some(command_tx) = self.command_tx.take() {
            let _ = command_tx.send(Command::Released { claim: self.claim });
        }
    }
}

#[derive(Clone)]
pub struct NodeCapacity {
    command_tx: Option<mpsc::UnboundedSender<Command>>,
    disabled_snapshot: NodeCapacitySnapshot,
}

#[derive(Debug, Clone, Copy, Default)]
struct CapacityClaim {
    cpu_slots: u32,
    unified_memory_mib: u32,
    accelerator_slots: u32,
}

impl From<&DeploymentResourceEstimateConfig> for CapacityClaim {
    fn from(value: &DeploymentResourceEstimateConfig) -> Self {
        Self {
            cpu_slots: value.cpu_slots,
            unified_memory_mib: value.unified_memory_mib,
            accelerator_slots: value.accelerator_slots,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct CapacityLimits {
    cpu_slots: Option<u32>,
    unified_memory_mib: Option<u32>,
    accelerator_slots: Option<u32>,
}

impl From<&AdmissionCapacityConfig> for CapacityLimits {
    fn from(value: &AdmissionCapacityConfig) -> Self {
        Self {
            cpu_slots: value.cpu_slots,
            unified_memory_mib: value.unified_memory_mib,
            accelerator_slots: value.accelerator_slots,
        }
    }
}

impl CapacityLimits {
    fn enabled(self) -> bool {
        self.cpu_slots.is_some()
            || self.unified_memory_mib.is_some()
            || self.accelerator_slots.is_some()
    }

    fn claim_error(self, claim: CapacityClaim) -> Option<NodeCapacityError> {
        [
            ("cpu_slots", claim.cpu_slots, self.cpu_slots),
            (
                "unified_memory_mib",
                claim.unified_memory_mib,
                self.unified_memory_mib,
            ),
            (
                "accelerator_slots",
                claim.accelerator_slots,
                self.accelerator_slots,
            ),
        ]
        .into_iter()
        .find_map(|(dimension, claim, limit)| {
            limit
                .filter(|limit| claim > *limit)
                .map(|_| NodeCapacityError::ClaimExceedsLimit { dimension })
        })
    }
}

impl NodeCapacity {
    pub fn from_config(config: &AdmissionCapacityConfig) -> Self {
        let limits = CapacityLimits::from(config);
        if !limits.enabled() {
            return Self {
                command_tx: None,
                disabled_snapshot: NodeCapacitySnapshot::default(),
            };
        }
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        tokio::spawn(run(
            command_rx,
            command_tx.clone(),
            limits,
            config.max_waiting_jobs,
            Duration::from_millis(config.priority_aging_ms),
        ));
        Self {
            command_tx: Some(command_tx),
            disabled_snapshot: NodeCapacitySnapshot::default(),
        }
    }

    pub async fn reserve(
        &self,
        job_id: String,
        estimate: &DeploymentResourceEstimateConfig,
        priority: Priority,
        deadline: Option<Instant>,
        cancellation: CancellationToken,
    ) -> Result<NodeCapacityReservation, NodeCapacityError> {
        let Some(command_tx) = &self.command_tx else {
            return Ok(NodeCapacityReservation {
                command_tx: None,
                claim: CapacityClaim::default(),
            });
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let claim = CapacityClaim::from(estimate);
        command_tx
            .send(Command::Enqueue(Ticket {
                job_id: job_id.clone(),
                claim,
                priority,
                enqueued_at: Instant::now(),
                reply_tx,
            }))
            .map_err(|_| NodeCapacityError::Unavailable)?;

        let result = match deadline {
            Some(deadline) => tokio::select! {
                result = reply_rx => result.unwrap_or(Err(NodeCapacityError::Unavailable)),
                _ = cancellation.cancelled() => Err(NodeCapacityError::Cancelled),
                _ = sleep_until(deadline) => Err(NodeCapacityError::DeadlineExpired),
            },
            None => tokio::select! {
                result = reply_rx => result.unwrap_or(Err(NodeCapacityError::Unavailable)),
                _ = cancellation.cancelled() => Err(NodeCapacityError::Cancelled),
            },
        };
        if result.is_err() {
            let _ = command_tx.send(Command::Cancel { job_id });
        }
        result
    }

    pub async fn snapshot(&self) -> Result<NodeCapacitySnapshot, NodeCapacityError> {
        let Some(command_tx) = &self.command_tx else {
            return Ok(self.disabled_snapshot.clone());
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        command_tx
            .send(Command::Snapshot { reply_tx })
            .map_err(|_| NodeCapacityError::Unavailable)?;
        reply_rx.await.map_err(|_| NodeCapacityError::Unavailable)
    }
}

struct Ticket {
    job_id: String,
    claim: CapacityClaim,
    priority: Priority,
    enqueued_at: Instant,
    reply_tx: oneshot::Sender<Result<NodeCapacityReservation, NodeCapacityError>>,
}

enum Command {
    Enqueue(Ticket),
    Cancel {
        job_id: String,
    },
    Released {
        claim: CapacityClaim,
    },
    Snapshot {
        reply_tx: oneshot::Sender<NodeCapacitySnapshot>,
    },
}

struct CapacityState {
    limits: CapacityLimits,
    used: CapacityClaim,
    interactive: VecDeque<Ticket>,
    normal: VecDeque<Ticket>,
    background: VecDeque<Ticket>,
}

impl CapacityState {
    fn pending(&self) -> usize {
        self.interactive.len() + self.normal.len() + self.background.len()
    }

    fn push(&mut self, ticket: Ticket) {
        match ticket.priority {
            Priority::Interactive => self.interactive.push_back(ticket),
            Priority::Normal => self.normal.push_back(ticket),
            Priority::Background => self.background.push_back(ticket),
        }
    }

    fn remove(&mut self, job_id: &str) -> Option<Ticket> {
        [
            &mut self.interactive,
            &mut self.normal,
            &mut self.background,
        ]
        .into_iter()
        .find_map(|queue| {
            queue
                .iter()
                .position(|ticket| ticket.job_id == job_id)
                .map(|index| queue.remove(index).expect("index exists"))
        })
    }

    fn fits(&self, claim: CapacityClaim) -> bool {
        fits(self.limits.cpu_slots, self.used.cpu_slots, claim.cpu_slots)
            && fits(
                self.limits.unified_memory_mib,
                self.used.unified_memory_mib,
                claim.unified_memory_mib,
            )
            && fits(
                self.limits.accelerator_slots,
                self.used.accelerator_slots,
                claim.accelerator_slots,
            )
    }

    fn reserve(&mut self, claim: CapacityClaim) {
        self.used.cpu_slots = self.used.cpu_slots.saturating_add(claim.cpu_slots);
        self.used.unified_memory_mib = self
            .used
            .unified_memory_mib
            .saturating_add(claim.unified_memory_mib);
        self.used.accelerator_slots = self
            .used
            .accelerator_slots
            .saturating_add(claim.accelerator_slots);
    }

    fn release(&mut self, claim: CapacityClaim) {
        self.used.cpu_slots = self.used.cpu_slots.saturating_sub(claim.cpu_slots);
        self.used.unified_memory_mib = self
            .used
            .unified_memory_mib
            .saturating_sub(claim.unified_memory_mib);
        self.used.accelerator_slots = self
            .used
            .accelerator_slots
            .saturating_sub(claim.accelerator_slots);
    }

    fn take_next_fitting(&mut self, aging_after: Duration) -> Option<Ticket> {
        let now = Instant::now();
        let candidates = [
            (0usize, self.interactive.iter().enumerate()),
            (1usize, self.normal.iter().enumerate()),
            (2usize, self.background.iter().enumerate()),
        ];
        let selected = candidates
            .into_iter()
            .flat_map(|(queue_index, tickets)| {
                tickets.map(move |(ticket_index, ticket)| (queue_index, ticket_index, ticket))
            })
            .filter(|(_, _, ticket)| self.fits(ticket.claim))
            .min_by(|(_, _, left), (_, _, right)| compare_ticket(left, right, now, aging_after))
            .map(|(queue_index, ticket_index, _)| (queue_index, ticket_index))?;
        match selected.0 {
            0 => self.interactive.remove(selected.1),
            1 => self.normal.remove(selected.1),
            2 => self.background.remove(selected.1),
            _ => unreachable!(),
        }
    }

    fn snapshot(&self) -> NodeCapacitySnapshot {
        NodeCapacitySnapshot {
            cpu_slots: dimension_snapshot(self.limits.cpu_slots, self.used.cpu_slots),
            unified_memory_mib: dimension_snapshot(
                self.limits.unified_memory_mib,
                self.used.unified_memory_mib,
            ),
            accelerator_slots: dimension_snapshot(
                self.limits.accelerator_slots,
                self.used.accelerator_slots,
            ),
            pending: Some(self.pending()),
        }
    }
}

fn fits(limit: Option<u32>, used: u32, claim: u32) -> bool {
    limit.is_none_or(|limit| used.saturating_add(claim) <= limit)
}

fn dimension_snapshot(limit: Option<u32>, used: u32) -> Option<CapacityDimensionSnapshot> {
    limit.map(|limit| CapacityDimensionSnapshot {
        limit,
        reserved: used.min(limit),
        available: limit.saturating_sub(used),
    })
}

fn compare_ticket(
    left: &Ticket,
    right: &Ticket,
    now: Instant,
    aging_after: Duration,
) -> std::cmp::Ordering {
    effective_rank(left, now, aging_after)
        .cmp(&effective_rank(right, now, aging_after))
        .then_with(|| left.enqueued_at.cmp(&right.enqueued_at))
}

fn effective_rank(ticket: &Ticket, now: Instant, aging_after: Duration) -> u128 {
    let base: u8 = match ticket.priority {
        Priority::Interactive => 0,
        Priority::Normal => 1,
        Priority::Background => 2,
    };
    base.saturating_sub(
        (now.duration_since(ticket.enqueued_at).as_millis() / aging_after.as_millis()) as u8,
    ) as u128
}

async fn run(
    mut command_rx: mpsc::UnboundedReceiver<Command>,
    command_tx: mpsc::UnboundedSender<Command>,
    limits: CapacityLimits,
    max_waiting_jobs: usize,
    aging_after: Duration,
) {
    let mut state = CapacityState {
        limits,
        used: CapacityClaim::default(),
        interactive: VecDeque::new(),
        normal: VecDeque::new(),
        background: VecDeque::new(),
    };
    while let Some(command) = command_rx.recv().await {
        match command {
            Command::Enqueue(ticket) if state.pending() >= max_waiting_jobs => {
                let _ = ticket.reply_tx.send(Err(NodeCapacityError::QueueFull));
            }
            Command::Enqueue(ticket) => {
                if let Some(error) = state.limits.claim_error(ticket.claim) {
                    let _ = ticket.reply_tx.send(Err(error));
                } else {
                    state.push(ticket);
                }
            }
            Command::Cancel { job_id } => {
                if let Some(ticket) = state.remove(&job_id) {
                    let _ = ticket.reply_tx.send(Err(NodeCapacityError::Cancelled));
                }
            }
            Command::Released { claim } => state.release(claim),
            Command::Snapshot { reply_tx } => {
                let _ = reply_tx.send(state.snapshot());
            }
        }
        while let Some(ticket) = state.take_next_fitting(aging_after) {
            state.reserve(ticket.claim);
            let reservation = NodeCapacityReservation {
                command_tx: Some(command_tx.clone()),
                claim: ticket.claim,
            };
            if ticket.reply_tx.send(Ok(reservation)).is_err() {
                // The dropped reservation sends `Released`; the next actor
                // turn makes the units available to another waiting ticket.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn config() -> AdmissionCapacityConfig {
        AdmissionCapacityConfig {
            max_waiting_jobs: 4,
            priority_aging_ms: 10_000,
            ..AdmissionCapacityConfig::default()
        }
    }

    #[tokio::test]
    async fn shared_capacity_blocks_then_releases_a_second_local_claim() {
        let capacity = NodeCapacity::from_config(&AdmissionCapacityConfig {
            unified_memory_mib: Some(100),
            ..config()
        });
        let claim = DeploymentResourceEstimateConfig {
            unified_memory_mib: 100,
            ..DeploymentResourceEstimateConfig::default()
        };
        let first = capacity
            .reserve(
                "first".into(),
                &claim,
                Priority::Normal,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            capacity
                .snapshot()
                .await
                .unwrap()
                .unified_memory_mib
                .unwrap()
                .available,
            0
        );

        let second_capacity = capacity.clone();
        let second_claim = claim.clone();
        let second = tokio::spawn(async move {
            second_capacity
                .reserve(
                    "second".into(),
                    &second_claim,
                    Priority::Normal,
                    None,
                    CancellationToken::new(),
                )
                .await
        });
        tokio::task::yield_now().await;
        assert!(!second.is_finished());
        drop(first);
        let second = second.await.unwrap().unwrap();
        drop(second);
        assert_eq!(
            capacity
                .snapshot()
                .await
                .unwrap()
                .unified_memory_mib
                .unwrap()
                .available,
            100
        );
    }

    #[tokio::test]
    async fn capacity_wait_honors_cancellation_and_deadline() {
        let capacity = NodeCapacity::from_config(&AdmissionCapacityConfig {
            cpu_slots: Some(1),
            ..config()
        });
        let claim = DeploymentResourceEstimateConfig {
            cpu_slots: 1,
            ..DeploymentResourceEstimateConfig::default()
        };
        let _first = capacity
            .reserve(
                "first".into(),
                &claim,
                Priority::Normal,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let cancellation = CancellationToken::new();
        let cancelled = cancellation.clone();
        let waiting_capacity = capacity.clone();
        let waiting_claim = claim.clone();
        let waiting = tokio::spawn(async move {
            waiting_capacity
                .reserve(
                    "cancel".into(),
                    &waiting_claim,
                    Priority::Normal,
                    None,
                    cancelled,
                )
                .await
        });
        tokio::task::yield_now().await;
        cancellation.cancel();
        assert!(matches!(
            waiting.await.unwrap(),
            Err(NodeCapacityError::Cancelled)
        ));

        let expired = capacity
            .reserve(
                "expired".into(),
                &claim,
                Priority::Normal,
                Some(Instant::now() + Duration::from_millis(1)),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(expired, Err(NodeCapacityError::DeadlineExpired)));
    }

    #[tokio::test]
    async fn capacity_favors_interactive_work_that_fits_the_same_budget() {
        let capacity = NodeCapacity::from_config(&AdmissionCapacityConfig {
            cpu_slots: Some(1),
            ..config()
        });
        let blocking = capacity
            .reserve(
                "blocking".into(),
                &DeploymentResourceEstimateConfig {
                    cpu_slots: 1,
                    ..DeploymentResourceEstimateConfig::default()
                },
                Priority::Normal,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let background_capacity = capacity.clone();
        let background = tokio::spawn(async move {
            background_capacity
                .reserve(
                    "background".into(),
                    &DeploymentResourceEstimateConfig {
                        cpu_slots: 1,
                        ..DeploymentResourceEstimateConfig::default()
                    },
                    Priority::Background,
                    None,
                    CancellationToken::new(),
                )
                .await
        });
        tokio::task::yield_now().await;
        let interactive_capacity = capacity.clone();
        let interactive = tokio::spawn(async move {
            interactive_capacity
                .reserve(
                    "interactive".into(),
                    &DeploymentResourceEstimateConfig {
                        cpu_slots: 1,
                        ..DeploymentResourceEstimateConfig::default()
                    },
                    Priority::Interactive,
                    None,
                    CancellationToken::new(),
                )
                .await
        });
        tokio::task::yield_now().await;
        drop(blocking);
        let interactive = interactive.await.unwrap().unwrap();
        assert!(!background.is_finished());
        drop(interactive);
        drop(background.await.unwrap().unwrap());
    }

    #[tokio::test]
    async fn capacity_rejects_a_claim_larger_than_its_configured_limit() {
        let capacity = NodeCapacity::from_config(&AdmissionCapacityConfig {
            accelerator_slots: Some(1),
            ..config()
        });
        let result = capacity
            .reserve(
                "oversize".into(),
                &DeploymentResourceEstimateConfig {
                    accelerator_slots: 2,
                    ..DeploymentResourceEstimateConfig::default()
                },
                Priority::Normal,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(
            result,
            Err(NodeCapacityError::ClaimExceedsLimit {
                dimension: "accelerator_slots"
            })
        ));
    }
}

//! In-memory, process-local counters for the M1 control plane.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};

use serde::Serialize;

use crate::scheduler::QueueSnapshot;

#[derive(Default)]
pub struct RuntimeMetrics {
    submitted: AtomicU64,
    dispatched: AtomicU64,
    succeeded: AtomicU64,
    failed: AtomicU64,
    cancelled: AtomicU64,
    expired: AtomicU64,
    queue_rejected: AtomicU64,
    queue_wait_ms: AtomicU64,
}

#[derive(Debug, Serialize)]
pub struct MetricsSnapshot {
    pub submitted: u64,
    pub dispatched: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub expired: u64,
    pub queue_rejected: u64,
    pub queue_wait_ms_total: u64,
    pub provider_queues: BTreeMap<String, ProviderQueueMetrics>,
}

#[derive(Debug, Serialize)]
pub struct ProviderQueueMetrics {
    pub pending_interactive: usize,
    pub pending_normal: usize,
    pub pending_background: usize,
    pub active: usize,
}

impl RuntimeMetrics {
    pub fn submitted(&self) {
        self.submitted.fetch_add(1, Ordering::Relaxed);
    }
    pub fn dispatched(&self, wait_ms: u64) {
        self.dispatched.fetch_add(1, Ordering::Relaxed);
        self.queue_wait_ms.fetch_add(wait_ms, Ordering::Relaxed);
    }
    pub fn succeeded(&self) {
        self.succeeded.fetch_add(1, Ordering::Relaxed);
    }
    pub fn failed(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }
    pub fn cancelled(&self) {
        self.cancelled.fetch_add(1, Ordering::Relaxed);
    }
    pub fn expired(&self) {
        self.expired.fetch_add(1, Ordering::Relaxed);
    }
    pub fn queue_rejected(&self) {
        self.queue_rejected.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self, provider_queues: BTreeMap<String, QueueSnapshot>) -> MetricsSnapshot {
        MetricsSnapshot {
            submitted: self.submitted.load(Ordering::Relaxed),
            dispatched: self.dispatched.load(Ordering::Relaxed),
            succeeded: self.succeeded.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            cancelled: self.cancelled.load(Ordering::Relaxed),
            expired: self.expired.load(Ordering::Relaxed),
            queue_rejected: self.queue_rejected.load(Ordering::Relaxed),
            queue_wait_ms_total: self.queue_wait_ms.load(Ordering::Relaxed),
            provider_queues: provider_queues
                .into_iter()
                .map(|(id, queue)| {
                    (
                        id,
                        ProviderQueueMetrics {
                            pending_interactive: queue.interactive,
                            pending_normal: queue.normal,
                            pending_background: queue.background,
                            active: queue.active,
                        },
                    )
                })
                .collect(),
        }
    }
}

//! Per-provider bounded priority scheduling with aging and cancellation.

use std::{cmp::Ordering, collections::VecDeque, sync::Arc, time::Duration};

use infer_core::Priority;
use thiserror::Error;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueSnapshot {
    pub interactive: usize,
    pub normal: usize,
    pub background: usize,
    pub active: usize,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerError {
    #[error("provider queue is full")]
    QueueFull,
    #[error("request deadline expired before execution started")]
    DeadlineExpired,
    #[error("response was cancelled before execution started")]
    Cancelled,
    #[error("provider scheduler is unavailable")]
    Unavailable,
}

/// A permit represents an active provider slot. Dropping it wakes the scheduler
/// so another queued request can be dispatched.
#[derive(Debug)]
pub struct ScheduledPermit {
    permit: Option<OwnedSemaphorePermit>,
    command_tx: mpsc::UnboundedSender<Command>,
}

impl Drop for ScheduledPermit {
    fn drop(&mut self) {
        self.permit.take();
        let _ = self.command_tx.send(Command::Released);
    }
}

#[derive(Clone)]
pub struct ProviderScheduler {
    command_tx: mpsc::UnboundedSender<Command>,
}

impl ProviderScheduler {
    pub fn new(max_concurrency: usize, max_queue: usize, aging_after: Duration) -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        tokio::spawn(run(
            command_rx,
            command_tx.clone(),
            Arc::new(Semaphore::new(max_concurrency)),
            max_queue,
            aging_after,
        ));
        Self { command_tx }
    }

    pub async fn acquire(
        &self,
        job_id: String,
        priority: Priority,
        deadline: Option<Instant>,
        cancellation: CancellationToken,
    ) -> Result<ScheduledPermit, SchedulerError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(Command::Enqueue(Ticket {
                job_id: job_id.clone(),
                priority,
                enqueued_at: Instant::now(),
                reply_tx,
            }))
            .map_err(|_| SchedulerError::Unavailable)?;

        let result = match deadline {
            Some(deadline) => {
                tokio::select! {
                    result = reply_rx => result.unwrap_or(Err(SchedulerError::Unavailable)),
                    _ = cancellation.cancelled() => Err(SchedulerError::Cancelled),
                    _ = sleep_until(deadline) => Err(SchedulerError::DeadlineExpired),
                }
            }
            None => {
                tokio::select! {
                    result = reply_rx => result.unwrap_or(Err(SchedulerError::Unavailable)),
                    _ = cancellation.cancelled() => Err(SchedulerError::Cancelled),
                }
            }
        };

        if result.is_err() {
            let _ = self.command_tx.send(Command::Cancel { job_id });
        }
        result
    }

    pub async fn snapshot(&self) -> Result<QueueSnapshot, SchedulerError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(Command::Snapshot { reply_tx })
            .map_err(|_| SchedulerError::Unavailable)?;
        reply_rx.await.map_err(|_| SchedulerError::Unavailable)
    }
}

struct Ticket {
    job_id: String,
    priority: Priority,
    enqueued_at: Instant,
    reply_tx: oneshot::Sender<Result<ScheduledPermit, SchedulerError>>,
}

enum Command {
    Enqueue(Ticket),
    Cancel {
        job_id: String,
    },
    Released,
    Snapshot {
        reply_tx: oneshot::Sender<QueueSnapshot>,
    },
}

struct Queues {
    interactive: VecDeque<Ticket>,
    normal: VecDeque<Ticket>,
    background: VecDeque<Ticket>,
    active: usize,
}

impl Queues {
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

    fn take_next(&mut self, aging_after: Duration) -> Option<Ticket> {
        let now = Instant::now();
        let options = [
            (0usize, self.interactive.front()),
            (1usize, self.normal.front()),
            (2usize, self.background.front()),
        ];
        let selected = options
            .into_iter()
            .filter_map(|(index, ticket)| ticket.map(|ticket| (index, ticket)))
            .min_by(|(_, left), (_, right)| compare_ticket(left, right, now, aging_after))
            .map(|(index, _)| index)?;
        match selected {
            0 => self.interactive.pop_front(),
            1 => self.normal.pop_front(),
            2 => self.background.pop_front(),
            _ => unreachable!(),
        }
    }

    fn snapshot(&self) -> QueueSnapshot {
        QueueSnapshot {
            interactive: self.interactive.len(),
            normal: self.normal.len(),
            background: self.background.len(),
            active: self.active,
        }
    }
}

fn compare_ticket(left: &Ticket, right: &Ticket, now: Instant, aging_after: Duration) -> Ordering {
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
    semaphore: Arc<Semaphore>,
    max_queue: usize,
    aging_after: Duration,
) {
    let mut queues = Queues {
        interactive: VecDeque::new(),
        normal: VecDeque::new(),
        background: VecDeque::new(),
        active: 0,
    };
    while let Some(command) = command_rx.recv().await {
        match command {
            Command::Enqueue(ticket) if queues.pending() >= max_queue => {
                let _ = ticket.reply_tx.send(Err(SchedulerError::QueueFull));
            }
            Command::Enqueue(ticket) => queues.push(ticket),
            Command::Cancel { job_id } => {
                if let Some(ticket) = queues.remove(&job_id) {
                    let _ = ticket.reply_tx.send(Err(SchedulerError::Cancelled));
                }
            }
            Command::Released => queues.active = queues.active.saturating_sub(1),
            Command::Snapshot { reply_tx } => {
                let _ = reply_tx.send(queues.snapshot());
            }
        }
        while let Ok(permit) = semaphore.clone().try_acquire_owned() {
            let Some(ticket) = queues.take_next(aging_after) else {
                drop(permit);
                break;
            };
            queues.active += 1;
            let scheduled = ScheduledPermit {
                permit: Some(permit),
                command_tx: command_tx.clone(),
            };
            if ticket.reply_tx.send(Ok(scheduled)).is_err() {
                // `ScheduledPermit::drop` releases both the semaphore slot and
                // sends `Released`, so the next actor turn fixes `active`.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_requests_above_the_bounded_queue() {
        let scheduler = ProviderScheduler::new(1, 1, Duration::from_secs(10));
        let active = scheduler
            .acquire(
                "active".into(),
                Priority::Normal,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let cancellation = CancellationToken::new();
        let waiting_scheduler = scheduler.clone();
        let waiting_cancellation = cancellation.clone();
        let waiting = tokio::spawn(async move {
            waiting_scheduler
                .acquire(
                    "waiting".into(),
                    Priority::Normal,
                    None,
                    waiting_cancellation,
                )
                .await
        });
        tokio::task::yield_now().await;
        assert_eq!(scheduler.snapshot().await.unwrap().normal, 1);
        let rejected = scheduler
            .acquire(
                "rejected".into(),
                Priority::Normal,
                None,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(rejected.unwrap_err(), SchedulerError::QueueFull);
        cancellation.cancel();
        assert_eq!(
            waiting.await.unwrap().unwrap_err(),
            SchedulerError::Cancelled
        );
        drop(active);
    }

    #[tokio::test]
    async fn expires_while_waiting_for_a_provider_slot() {
        let scheduler = ProviderScheduler::new(1, 2, Duration::from_secs(10));
        let active = scheduler
            .acquire(
                "active".into(),
                Priority::Normal,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_millis(10);
        let result = scheduler
            .acquire(
                "expired".into(),
                Priority::Interactive,
                Some(deadline),
                CancellationToken::new(),
            )
            .await;
        assert_eq!(result.unwrap_err(), SchedulerError::DeadlineExpired);
        drop(active);
    }

    #[tokio::test]
    async fn aged_background_work_beats_newer_interactive_work() {
        let scheduler = ProviderScheduler::new(1, 4, Duration::from_millis(5));
        let active = scheduler
            .acquire(
                "active".into(),
                Priority::Normal,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let background_scheduler = scheduler.clone();
        let background = tokio::spawn(async move {
            background_scheduler
                .acquire(
                    "background".into(),
                    Priority::Background,
                    None,
                    CancellationToken::new(),
                )
                .await
        });
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(15)).await;
        let interactive_scheduler = scheduler.clone();
        let interactive = tokio::spawn(async move {
            interactive_scheduler
                .acquire(
                    "interactive".into(),
                    Priority::Interactive,
                    None,
                    CancellationToken::new(),
                )
                .await
        });
        tokio::task::yield_now().await;
        drop(active);
        let background_permit = background.await.unwrap().unwrap();
        let snapshot = scheduler.snapshot().await.unwrap();
        assert_eq!(snapshot.background, 0);
        assert_eq!(snapshot.interactive, 1);
        drop(background_permit);
        interactive.await.unwrap().unwrap();
    }
}

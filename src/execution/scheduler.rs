

use crate::execution::state_machine::{ExecutionStage, ExecutionStateMachine, OperationId};
use crate::types::{AtomicInstant, DigestInfo, Result};
use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// Entry for in-flight actions with timestamp for stale detection
#[derive(Debug, Clone)]
struct InFlightEntry {
    operation_id: OperationId,
    inserted_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuePriority {
    #[allow(dead_code)]
    Fast,
    Medium,
    #[allow(dead_code)]
    Slow,
}

impl QueuePriority {
    #[allow(dead_code)]
    pub fn from_action(action: &ExecutableAction) -> Self {

        let total_size: i64 = action.input_digests.iter().map(|d| d.size).sum();

        if total_size < 1024 * 1024 {

            QueuePriority::Fast
        } else if total_size < 100 * 1024 * 1024 {

            QueuePriority::Medium
        } else {

            QueuePriority::Slow
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExecutableAction {
    pub operation_id: OperationId,
    pub action_digest: DigestInfo,
    #[allow(dead_code)]
    pub input_digests: Vec<DigestInfo>,
    pub command: Vec<String>,
    pub timeout: Duration,
    pub priority: QueuePriority,
    pub output_files: Vec<String>,
    pub output_directories: Vec<String>,
    pub working_directory: Option<String>,
    pub input_root_digest: Option<DigestInfo>,
}

struct QueueItem {
    action: ExecutableAction,
    #[allow(dead_code)]
    enqueued_at: std::time::Instant,
    state_machine: Arc<ExecutionStateMachine>,
}

pub struct MultiLevelScheduler {

    fast_queue: Mutex<VecDeque<QueueItem>>,

    medium_queue: Mutex<VecDeque<QueueItem>>,

    slow_queue: Mutex<VecDeque<QueueItem>>,

    /// In-flight actions with timestamp for stale detection (memory leak prevention)
    in_flight_actions: Arc<DashMap<DigestInfo, InFlightEntry>>,

    stats: Arc<SchedulerStats>,

    /// Notify workers when new work is available (eliminates polling)
    work_notify: Notify,
}

#[derive(Debug, Default)]
struct SchedulerStats {
    fast_enqueued: AtomicInstant,
    medium_enqueued: AtomicInstant,
    slow_enqueued: AtomicInstant,
    fast_dequeued: AtomicInstant,
    medium_dequeued: AtomicInstant,
    slow_dequeued: AtomicInstant,
    #[allow(dead_code)]
    merged_count: AtomicInstant,
}

impl MultiLevelScheduler {
    pub fn new() -> Self {
        Self {
            fast_queue: Mutex::new(VecDeque::with_capacity(10000)),
            medium_queue: Mutex::new(VecDeque::with_capacity(10000)),
            slow_queue: Mutex::new(VecDeque::with_capacity(10000)),
            in_flight_actions: Arc::new(DashMap::new()),
            stats: Arc::new(SchedulerStats::default()),
            work_notify: Notify::new(),
        }
    }

    pub fn enqueue(
        &self,
        action: ExecutableAction,
        state_machine: Arc<ExecutionStateMachine>,
    ) -> Result<OperationId> {
        let digest = action.action_digest;

        if let Some(existing) = self.in_flight_actions.get(&digest) {
            let existing_entry = existing.value();
            warn!(
                "Action merging: digest {:?} already being processed as {}",
                digest, existing_entry.operation_id
            );
            return Ok(existing_entry.operation_id);
        }

        self.in_flight_actions.insert(digest, InFlightEntry {
            operation_id: action.operation_id,
            inserted_at: Instant::now(),
        });

        let priority = action.priority;
        let operation_id = action.operation_id;

        let item = QueueItem {
            action,
            enqueued_at: std::time::Instant::now(),
            state_machine,
        };

        match priority {
            QueuePriority::Fast => {
                let mut queue = self.fast_queue.lock();
                queue.push_back(item);
                self.stats.fast_enqueued.refresh();
                debug!("Enqueued to fast queue: {}", operation_id);
            }
            QueuePriority::Medium => {
                let mut queue = self.medium_queue.lock();
                queue.push_back(item);
                self.stats.medium_enqueued.refresh();
                debug!("Enqueued to medium queue: {}", operation_id);
            }
            QueuePriority::Slow => {
                let mut queue = self.slow_queue.lock();
                queue.push_back(item);
                self.stats.slow_enqueued.refresh();
                debug!("Enqueued to slow queue: {}", operation_id);
            }
        }

        self.work_notify.notify_one();

        Ok(operation_id)
    }

    pub fn dequeue(&self) -> Option<(ExecutableAction, Arc<ExecutionStateMachine>)> {

        if fastrand::u8(0..100) < 70 {
            if let Some(item) = self.try_dequeue_fast() {
                return Some((item.action, item.state_machine));
            }
        }

        if fastrand::u8(0..100) < 83 {
            if let Some(item) = self.try_dequeue_medium() {
                return Some((item.action, item.state_machine));
            }
        }

        if let Some(item) = self.try_dequeue_slow() {
            return Some((item.action, item.state_machine));
        }

        self.try_dequeue_fast()
            .or_else(|| self.try_dequeue_medium())
            .or_else(|| self.try_dequeue_slow())
            .map(|item| (item.action, item.state_machine))
    }

    fn try_dequeue_fast(&self) -> Option<QueueItem> {
        let mut queue = self.fast_queue.lock();
        let item = queue.pop_front()?;
        self.stats.fast_dequeued.refresh();
        Some(item)
    }

    fn try_dequeue_medium(&self) -> Option<QueueItem> {
        let mut queue = self.medium_queue.lock();
        let item = queue.pop_front()?;
        self.stats.medium_dequeued.refresh();
        Some(item)
    }

    fn try_dequeue_slow(&self) -> Option<QueueItem> {
        let mut queue = self.slow_queue.lock();
        let item = queue.pop_front()?;
        self.stats.slow_dequeued.refresh();
        Some(item)
    }

    pub fn complete_action(&self, digest: &DigestInfo) {
        self.in_flight_actions.remove(digest);
        debug!("Action completed and removed from in_flight: {:?}", digest);
    }

    #[allow(dead_code)]
    pub fn stats(&self) -> SchedulerQueueStats {
        SchedulerQueueStats {
            fast_queued: self.fast_queue.lock().len(),
            medium_queued: self.medium_queue.lock().len(),
            slow_queued: self.slow_queue.lock().len(),
            in_flight: self.in_flight_actions.len(),
        }
    }

    #[allow(dead_code)]
    pub fn has_work(&self) -> bool {
        !self.fast_queue.lock().is_empty()
            || !self.medium_queue.lock().is_empty()
            || !self.slow_queue.lock().is_empty()
    }

    /// Cleanup stale in-flight actions to prevent memory leaks.
    /// Called by the execution engine's cleanup loop.
    pub fn cleanup_stale_actions(&self, max_age: Duration) -> usize {
        let now = Instant::now();
        let mut removed = 0;
        
        let stale_keys: Vec<DigestInfo> = self
            .in_flight_actions
            .iter()
            .filter(|entry| now.duration_since(entry.value().inserted_at) > max_age)
            .map(|entry| *entry.key())
            .collect();
        
        for key in stale_keys {
            if self.in_flight_actions.remove(&key).is_some() {
                removed += 1;
                warn!("Removed stale in-flight action: {:?}", key);
            }
        }
        
        removed
    }

    /// Wait until work is available, then return it.
    /// This is the event-driven alternative to polling.
    #[allow(dead_code)]
    pub async fn wait_for_work(&self) -> Option<(ExecutableAction, Arc<ExecutionStateMachine>)> {
        if let Some(work) = self.dequeue() {
            return Some(work);
        }

        loop {
            let notify_fut = self.work_notify.notified();
            tokio::pin!(notify_fut);

            match tokio::time::timeout(Duration::from_secs(1), notify_fut).await {
                Ok(_) => {
                    if let Some(work) = self.dequeue() {
                        return Some(work);
                    }
                }
                Err(_) => {
                    if let Some(work) = self.dequeue() {
                        return Some(work);
                    }
                }
            }
        }
    }
}

impl Default for MultiLevelScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct SchedulerQueueStats {
    pub fast_queued: usize,
    pub medium_queued: usize,
    pub slow_queued: usize,
    pub in_flight: usize,
}

impl fmt::Display for SchedulerQueueStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Scheduler[fast={}, medium={}, slow={}, in_flight={}]",
            self.fast_queued, self.medium_queued, self.slow_queued, self.in_flight
        )
    }
}

use std::fmt;

#[allow(dead_code)]
pub struct ExecutionWorkerPool {
    scheduler: Arc<MultiLevelScheduler>,
    worker_count: usize,
}

#[allow(dead_code)]
impl ExecutionWorkerPool {
    pub fn new(scheduler: Arc<MultiLevelScheduler>, worker_count: usize) -> Self {
        Self {
            scheduler,
            worker_count,
        }
    }

    #[allow(dead_code)]
    pub fn spawn_workers(self: Arc<Self>) {
        for i in 0..self.worker_count {
            let scheduler = self.scheduler.clone();
            tokio::spawn(async move {
                info!("Execution worker {} started (event-driven)", i);
                loop {
                    if let Some((action, state_machine)) = scheduler.wait_for_work().await {
                        debug!("Worker {} processing action {}", i, action.operation_id);

                        if let Err(e) = state_machine.transition_to(ExecutionStage::Assigned).await
                        {
                            warn!("Failed to transition to Assigned: {}", e);
                            continue;
                        }

                        tokio::time::sleep(Duration::from_millis(100)).await;

                        scheduler.complete_action(&action.action_digest);

                        let _ = state_machine.transition_to(ExecutionStage::Completed).await;
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scheduler_enqueue_dequeue() {
        let scheduler = MultiLevelScheduler::new();
        let digest = DigestInfo::new("test", 1024);

        let action = ExecutableAction {
            operation_id: OperationId::generate(),
            action_digest: digest,
            input_digests: vec![],
            command: vec!["echo".to_string(), "hello".to_string()],
            timeout: Duration::from_secs(60),
            priority: QueuePriority::Fast,
            output_files: vec![],
            output_directories: vec![],
            working_directory: None,
            input_root_digest: None,
        };

        let sm = Arc::new(ExecutionStateMachine::new(action.operation_id, digest));

        let op_id = scheduler.enqueue(action, sm).unwrap();
        assert!(scheduler.has_work());

        let stats = scheduler.stats();
        assert_eq!(stats.fast_queued, 1);

        let (dequeued, _) = scheduler.dequeue().unwrap();
        assert_eq!(dequeued.operation_id, op_id);
    }

    #[test]
    fn test_action_merging() {
        let scheduler = MultiLevelScheduler::new();
        let digest = DigestInfo::new("same_action", 2048);

        let action1 = ExecutableAction {
            operation_id: OperationId::generate(),
            action_digest: digest,
            input_digests: vec![],
            command: vec!["cmd1".to_string()],
            timeout: Duration::from_secs(60),
            priority: QueuePriority::Fast,
            output_files: vec![],
            output_directories: vec![],
            working_directory: None,
            input_root_digest: None,
        };

        let action2 = ExecutableAction {
            operation_id: OperationId::generate(),
            action_digest: digest,
            input_digests: vec![],
            command: vec!["cmd2".to_string()],
            timeout: Duration::from_secs(60),
            priority: QueuePriority::Fast,
            output_files: vec![],
            output_directories: vec![],
            working_directory: None,
            input_root_digest: None,
        };

        let sm1 = Arc::new(ExecutionStateMachine::new(action1.operation_id, digest));
        let sm2 = Arc::new(ExecutionStateMachine::new(action2.operation_id, digest));

        let op_id1 = scheduler.enqueue(action1, sm1).unwrap();
        let op_id2 = scheduler.enqueue(action2, sm2).unwrap();

        assert_eq!(op_id1, op_id2);

        let stats = scheduler.stats();
        assert_eq!(stats.fast_queued, 1);
    }

    #[test]
    fn test_priority_from_action_size() {
        let small_action = ExecutableAction {
            operation_id: OperationId::generate(),
            action_digest: DigestInfo::new("small", 1024),
            input_digests: vec![DigestInfo::new("input", 512)],
            command: vec![],
            timeout: Duration::from_secs(60),
            priority: QueuePriority::Fast,
            output_files: vec![],
            output_directories: vec![],
            working_directory: None,
            input_root_digest: None,
        };

        let large_action = ExecutableAction {
            operation_id: OperationId::generate(),
            action_digest: DigestInfo::new("large", 1024),
            input_digests: vec![DigestInfo::new("input", 200 * 1024 * 1024)],
            command: vec![],
            timeout: Duration::from_secs(60),
            output_files: vec![],
            output_directories: vec![],
            working_directory: None,
            input_root_digest: None,
            priority: QueuePriority::Fast,
        };

        assert_eq!(
            QueuePriority::from_action(&small_action),
            QueuePriority::Fast
        );
        assert_eq!(
            QueuePriority::from_action(&large_action),
            QueuePriority::Slow
        );
    }
}

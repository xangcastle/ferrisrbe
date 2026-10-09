//! Priority queues for executable actions, fair across the invocations
//! that queue them, with de-duplication of identical actions already in
//! flight.

use crate::execution::state_machine::{ExecutionStage, ExecutionStateMachine, OperationId};
use crate::proto::build::bazel::remote::execution::v2::RequestMetadata;
use crate::types::{AtomicInstant, DigestInfo, Result};
use crate::worker::k8s::{NetworkPolicy, WorkerRequirements};
use dashmap::{mapref::entry::Entry, DashMap};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// Upper bound on how long [`MultiLevelScheduler::wait_for_work`] sleeps
/// between checks, so a missed notification never stalls a worker.
const WAIT_RECHECK_INTERVAL: Duration = Duration::from_secs(1);

/// Each 100 points of `ExecutionPolicy.priority` halve an invocation's share
/// of the workers against one with the lower value (Buildbarn's weighting).
const PRIORITY_DOUBLING: f64 = 100.0;

/// The operation that owns an in-flight digest and when it was admitted.
#[derive(Debug, Clone)]
struct InFlightEntry {
    operation_id: OperationId,
    inserted_at: Instant,
    /// The invocation counted as running it while it is out of the queues.
    invocation: Arc<str>,
    dispatched: bool,
}

/// The invocation an action belongs to for fair scheduling: the
/// `tool_invocation_id` of the `Execute` that queued it, or `""` without one.
fn invocation_of(action: &ExecutableAction) -> Arc<str> {
    Arc::from(
        action
            .metadata
            .as_ref()
            .map_or("", |m| m.tool_invocation_id.as_str()),
    )
}

/// The actions one invocation has waiting in a queue, and when the
/// invocation started waiting there, which breaks ties between invocations.
struct InvocationQueue {
    items: VecDeque<QueueItem>,
    since: u64,
}

/// One scheduling class, served fairly across invocations: the next action
/// comes from the invocation with the lowest
/// `(running + 1) * 2^(priority / 100)`, where `running` counts its actions
/// out of the queues and `priority` is the `ExecutionPolicy.priority` of its
/// first queued action (lower runs sooner, as in REAPI). Equal scores go to
/// the invocation waiting longest; within an invocation, actions keep their
/// order. A single invocation is served first in, first out.
#[derive(Default)]
struct FairQueue {
    invocations: HashMap<Arc<str>, InvocationQueue>,
    len: usize,
    next_since: u64,
}

impl FairQueue {
    fn push(&mut self, item: QueueItem, front: bool) {
        let key = invocation_of(&item.action);
        let since = self.next_since;
        let queue = self
            .invocations
            .entry(key)
            .or_insert_with(|| InvocationQueue {
                items: VecDeque::new(),
                since,
            });
        if queue.items.is_empty() {
            queue.since = since;
            self.next_since += 1;
        }
        if front {
            queue.items.push_front(item);
        } else {
            queue.items.push_back(item);
        }
        self.len += 1;
    }

    fn pop(&mut self, running: &HashMap<Arc<str>, usize>) -> Option<QueueItem> {
        let score = |key: &Arc<str>, queue: &InvocationQueue| {
            let priority = queue
                .items
                .front()
                .map_or(0, |i| i.action.execution_priority);
            let running = running.get(key).copied().unwrap_or(0) as f64;
            (running + 1.0) * 2f64.powf(f64::from(priority) / PRIORITY_DOUBLING)
        };
        let key = self
            .invocations
            .iter()
            .min_by(|(ka, a), (kb, b)| {
                score(ka, a)
                    .total_cmp(&score(kb, b))
                    .then(a.since.cmp(&b.since))
            })
            .map(|(key, _)| key.clone())?;
        let queue = self.invocations.get_mut(&key)?;
        let item = queue.items.pop_front()?;
        if queue.items.is_empty() {
            self.invocations.remove(&key);
        }
        self.len -= 1;
        Some(item)
    }

    fn len(&self) -> usize {
        self.len
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Takes out every item `remove` selects, keeping the order of the rest.
    fn drain_where(&mut self, mut remove: impl FnMut(&QueueItem) -> bool) -> Vec<QueueItem> {
        let mut removed = Vec::new();
        self.invocations.retain(|_, queue| {
            let (gone, kept): (VecDeque<QueueItem>, VecDeque<QueueItem>) =
                queue.items.drain(..).partition(|item| remove(item));
            removed.extend(gone);
            queue.items = kept;
            !queue.items.is_empty()
        });
        self.len -= removed.len();
        removed
    }
}

/// Scheduling class of an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuePriority {
    /// Served before every other queue: actions the client marks as being on
    /// the build's critical path with the `Priority=critical` platform property.
    Critical,
    /// Every other action.
    Medium,
}

impl QueuePriority {
    /// The class an action asks for with the `Priority` platform property:
    /// `critical` is [`QueuePriority::Critical`], and no property is `Medium`.
    ///
    /// # Errors
    ///
    /// For any other `Priority` value, so a typo is refused rather than
    /// silently scheduled as an ordinary action.
    pub fn from_platform(properties: &[(String, String)]) -> std::result::Result<Self, String> {
        match properties.iter().find(|(name, _)| name == "Priority") {
            None => Ok(QueuePriority::Medium),
            Some((_, value)) if value == "critical" => Ok(QueuePriority::Critical),
            Some((_, value)) => Err(format!(
                "unsupported Priority {value:?}; supported: critical"
            )),
        }
    }
}

/// Everything a worker needs to run one action.
#[derive(Debug, Clone)]
pub struct ExecutableAction {
    pub operation_id: OperationId,
    pub action_digest: DigestInfo,
    pub command: Vec<String>,
    pub timeout: Duration,
    pub priority: QueuePriority,
    pub output_files: Vec<String>,
    pub output_directories: Vec<String>,
    pub working_directory: Option<String>,
    pub input_root_digest: Option<DigestInfo>,
    /// `Command.environment_variables`, in the order the client sent them.
    pub environment_variables: Vec<(String, String)>,
    /// `Action.do_not_cache`: the result must not be written to the Action Cache.
    pub do_not_cache: bool,
    /// What a worker needs to run the action, from its REAPI platform.
    pub requirements: WorkerRequirements,
    pub network: NetworkPolicy,
    /// Workers lost while running this action; see `ExecutionEngine`.
    pub worker_losses: u32,
    /// `ExecutionPolicy.priority`: lower runs sooner, `0` is the default.
    pub execution_priority: i32,
    /// `RequestMetadata` of the `Execute` call that enqueued the action, which
    /// attributes its metrics and inputs; callers that join it later are not
    /// recorded here.
    pub metadata: Option<RequestMetadata>,
}

struct QueueItem {
    action: ExecutableAction,
    #[allow(dead_code)]
    enqueued_at: std::time::Instant,
    state_machine: Arc<ExecutionStateMachine>,
}

/// Critical and medium queues, critical first, each one a [`FairQueue`]
/// across invocations.
///
/// Identical actions are merged: while a digest is in flight, enqueueing it
/// again returns the operation already running it instead of a new one. The
/// digest stays in flight until [`MultiLevelScheduler::complete_action`] or
/// until [`MultiLevelScheduler::cleanup_stale_actions`] expires it.
pub struct MultiLevelScheduler {
    critical_queue: Mutex<FairQueue>,
    medium_queue: Mutex<FairQueue>,
    in_flight_actions: Arc<DashMap<DigestInfo, InFlightEntry>>,
    /// Actions of each invocation out of the queues: dequeued and neither
    /// put back nor completed.
    running: Mutex<HashMap<Arc<str>, usize>>,
    stats: Arc<SchedulerStats>,
    /// Signalled on every enqueue, waking one [`MultiLevelScheduler::wait_for_work`] caller.
    work_notify: Notify,
}

/// Time of the last enqueue and dequeue per queue.
#[derive(Debug, Default)]
struct SchedulerStats {
    critical_enqueued: AtomicInstant,
    critical_dequeued: AtomicInstant,
    medium_enqueued: AtomicInstant,
    medium_dequeued: AtomicInstant,
    #[allow(dead_code)]
    merged_count: AtomicInstant,
}

impl MultiLevelScheduler {
    pub fn new() -> Self {
        Self {
            critical_queue: Mutex::new(FairQueue::default()),
            medium_queue: Mutex::new(FairQueue::default()),
            in_flight_actions: Arc::new(DashMap::new()),
            running: Mutex::new(HashMap::new()),
            stats: Arc::new(SchedulerStats::default()),
            work_notify: Notify::new(),
        }
    }

    fn queue(&self, priority: QueuePriority) -> &Mutex<FairQueue> {
        match priority {
            QueuePriority::Critical => &self.critical_queue,
            QueuePriority::Medium => &self.medium_queue,
        }
    }

    fn enqueued_stat(&self, priority: QueuePriority) -> &AtomicInstant {
        match priority {
            QueuePriority::Critical => &self.stats.critical_enqueued,
            QueuePriority::Medium => &self.stats.medium_enqueued,
        }
    }

    fn dequeued_stat(&self, priority: QueuePriority) -> &AtomicInstant {
        match priority {
            QueuePriority::Critical => &self.stats.critical_dequeued,
            QueuePriority::Medium => &self.stats.medium_dequeued,
        }
    }

    /// Queues `action` at its priority and returns the operation that will
    /// run it: `action.operation_id`, or the id of the operation already
    /// running the same digest.
    pub fn enqueue(
        &self,
        action: ExecutableAction,
        state_machine: Arc<ExecutionStateMachine>,
    ) -> Result<OperationId> {
        let digest = action.action_digest;

        match self.in_flight_actions.entry(digest) {
            Entry::Occupied(existing) => {
                let existing_entry = existing.get();
                warn!(
                    "Action merging: digest {:?} already being processed as {}",
                    digest, existing_entry.operation_id
                );
                return Ok(existing_entry.operation_id);
            }
            Entry::Vacant(slot) => {
                slot.insert(InFlightEntry {
                    operation_id: action.operation_id,
                    inserted_at: Instant::now(),
                    invocation: invocation_of(&action),
                    dispatched: false,
                });
            }
        }

        let priority = action.priority;
        let operation_id = action.operation_id;
        self.queue(priority).lock().push(
            QueueItem {
                action,
                enqueued_at: std::time::Instant::now(),
                state_machine,
            },
            false,
        );
        self.enqueued_stat(priority).refresh();
        debug!("Enqueued to {:?} queue: {}", priority, operation_id);

        self.work_notify.notify_one();
        Ok(operation_id)
    }

    /// Puts back, at the front of its queue, an action that was dequeued but
    /// never ran (no idle worker matched it, the assignment could not be
    /// delivered, or its worker was lost). Unlike [`enqueue`], it does not
    /// merge with the in-flight entry, which is this same action.
    ///
    /// [`enqueue`]: MultiLevelScheduler::enqueue
    pub fn requeue(&self, action: ExecutableAction, state_machine: Arc<ExecutionStateMachine>) {
        self.put_back(action, state_machine);
        self.work_notify.notify_one();
    }

    /// [`requeue`] without signalling new work: for the dispatcher returning
    /// actions no idle worker matched, which nothing new could run yet.
    ///
    /// [`requeue`]: MultiLevelScheduler::requeue
    pub fn put_back(&self, action: ExecutableAction, state_machine: Arc<ExecutionStateMachine>) {
        self.undispatch(&action.action_digest);
        let priority = action.priority;
        self.queue(priority).lock().push(
            QueueItem {
                action,
                enqueued_at: std::time::Instant::now(),
                state_machine,
            },
            true,
        );
        self.enqueued_stat(priority).refresh();
    }

    /// Counts `digest`'s action as running for its invocation again no more:
    /// it is back in a queue or finished.
    fn undispatch(&self, digest: &DigestInfo) {
        let Some(mut entry) = self.in_flight_actions.get_mut(digest) else {
            return;
        };
        if std::mem::take(&mut entry.dispatched) {
            let invocation = entry.invocation.clone();
            drop(entry);
            let mut running = self.running.lock();
            if let Some(count) = running.get_mut(&invocation) {
                *count -= 1;
                if *count == 0 {
                    running.remove(&invocation);
                }
            }
        }
    }

    /// Takes out of the queues every action no client has followed for
    /// longer than `grace`. Their digests stay in flight: the caller
    /// finishes each operation and then completes it.
    pub fn remove_unwatched(
        &self,
        grace: Duration,
    ) -> Vec<(ExecutableAction, Arc<ExecutionStateMachine>)> {
        [QueuePriority::Critical, QueuePriority::Medium]
            .into_iter()
            .flat_map(|priority| {
                self.queue(priority)
                    .lock()
                    .drain_where(|item| item.state_machine.unwatched_for(grace))
            })
            .map(|item| (item.action, item.state_machine))
            .collect()
    }

    /// Returns once an action was enqueued or requeued since the last call;
    /// one signalled while nobody waits is kept for the next call.
    pub async fn work_queued(&self) {
        self.work_notify.notified().await;
    }

    /// Takes the next action, if any: the critical queue always goes first.
    pub fn dequeue(&self) -> Option<(ExecutableAction, Arc<ExecutionStateMachine>)> {
        self.try_dequeue(QueuePriority::Critical)
            .or_else(|| self.try_dequeue(QueuePriority::Medium))
            .map(|item| (item.action, item.state_machine))
    }

    fn try_dequeue(&self, priority: QueuePriority) -> Option<QueueItem> {
        let mut queue = self.queue(priority).lock();
        let mut running = self.running.lock();
        let item = queue.pop(&running)?;
        if let Some(mut entry) = self.in_flight_actions.get_mut(&item.action.action_digest) {
            if !entry.dispatched {
                entry.dispatched = true;
                *running.entry(entry.invocation.clone()).or_default() += 1;
            }
        }
        drop(running);
        drop(queue);
        self.dequeued_stat(priority).refresh();
        Some(item)
    }

    /// Releases `digest`, so the next identical action is scheduled anew.
    pub fn complete_action(&self, digest: &DigestInfo) {
        self.undispatch(digest);
        self.in_flight_actions.remove(digest);
        debug!("Action completed and removed from in_flight: {:?}", digest);
    }

    /// Actions waiting in every queue.
    pub fn queued_len(&self) -> usize {
        [QueuePriority::Critical, QueuePriority::Medium]
            .into_iter()
            .map(|priority| self.queue(priority).lock().len())
            .sum()
    }

    pub fn stats(&self) -> SchedulerQueueStats {
        SchedulerQueueStats {
            critical_queued: self.critical_queue.lock().len(),
            medium_queued: self.medium_queue.lock().len(),
            in_flight: self.in_flight_actions.len(),
        }
    }

    #[allow(dead_code)]
    pub fn has_work(&self) -> bool {
        !self.critical_queue.lock().is_empty() || !self.medium_queue.lock().is_empty()
    }

    /// Releases every in-flight digest admitted more than `max_age` ago and
    /// returns how many were released. Bounds the in-flight map when an
    /// operation never reports completion.
    pub fn cleanup_stale_actions(&self, max_age: Duration) -> usize {
        let now = Instant::now();
        let stale_keys: Vec<DigestInfo> = self
            .in_flight_actions
            .iter()
            .filter(|entry| now.duration_since(entry.value().inserted_at) > max_age)
            .map(|entry| *entry.key())
            .collect();

        let mut removed = 0;
        for key in stale_keys {
            self.undispatch(&key);
            if self.in_flight_actions.remove(&key).is_some() {
                removed += 1;
                warn!("Removed stale in-flight action: {:?}", key);
            }
        }
        removed
    }

    /// Waits until an action is available and returns it. Woken by each
    /// enqueue, and rechecks at least every second.
    #[allow(dead_code)]
    pub async fn wait_for_work(&self) -> Option<(ExecutableAction, Arc<ExecutionStateMachine>)> {
        loop {
            if let Some(work) = self.dequeue() {
                return Some(work);
            }
            let notified = self.work_notify.notified();
            tokio::pin!(notified);
            let _ = tokio::time::timeout(WAIT_RECHECK_INTERVAL, notified).await;
        }
    }
}

impl Default for MultiLevelScheduler {
    fn default() -> Self {
        Self::new()
    }
}

/// Queue depths and in-flight count at one instant.
#[derive(Debug, Clone)]
pub struct SchedulerQueueStats {
    pub critical_queued: usize,
    pub medium_queued: usize,
    pub in_flight: usize,
}

impl fmt::Display for SchedulerQueueStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Scheduler[critical={}, medium={}, in_flight={}]",
            self.critical_queued, self.medium_queued, self.in_flight
        )
    }
}

/// In-process consumers that drain the scheduler and complete each action
/// after a fixed delay. Used to exercise scheduling without real workers.
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

    fn action(digest: DigestInfo, command: &str) -> ExecutableAction {
        ExecutableAction {
            operation_id: OperationId::generate(),
            action_digest: digest,
            command: vec![command.to_string()],
            timeout: Duration::from_secs(60),
            priority: QueuePriority::Medium,
            output_files: vec![],
            output_directories: vec![],
            working_directory: None,
            input_root_digest: None,
            environment_variables: vec![],
            do_not_cache: false,
            requirements: WorkerRequirements::default(),
            network: NetworkPolicy::default(),
            worker_losses: 0,
            metadata: None,
            execution_priority: 0,
        }
    }

    fn digest(hash: &str, size: i64) -> DigestInfo {
        DigestInfo::new(hash, size).unwrap()
    }

    /// Enqueues `count` distinct actions of `invocation` at `priority`.
    fn enqueue_many(
        scheduler: &MultiLevelScheduler,
        invocation: &str,
        priority: i32,
        count: usize,
    ) {
        for n in 0..count {
            let hash = format!("{:064x}", fastrand::u64(..) as u128 * 1_000 + n as u128);
            let mut action = action(digest(&hash, 1), invocation);
            action.priority = QueuePriority::Medium;
            action.execution_priority = priority;
            action.metadata = Some(RequestMetadata {
                tool_invocation_id: invocation.to_string(),
                ..Default::default()
            });
            let sm = Arc::new(ExecutionStateMachine::new(
                action.operation_id,
                action.action_digest,
            ));
            scheduler.enqueue(action, sm).unwrap();
        }
    }

    /// The invocations of the next `count` dequeued actions, which stay
    /// dispatched.
    fn dequeue_invocations(scheduler: &MultiLevelScheduler, count: usize) -> Vec<String> {
        (0..count)
            .map(|_| scheduler.dequeue().unwrap().0.command[0].clone())
            .collect()
    }

    #[test]
    fn a_small_build_queued_behind_a_large_one_is_served_alongside_it() {
        let scheduler = MultiLevelScheduler::new();
        enqueue_many(&scheduler, "large", 0, 200);
        enqueue_many(&scheduler, "small", 0, 2);

        assert_eq!(
            dequeue_invocations(&scheduler, 4),
            ["large", "small", "large", "small"],
            "first in, first out would serve the 200 large actions first"
        );
    }

    #[test]
    fn a_hundred_more_priority_points_halve_an_invocations_share() {
        let scheduler = MultiLevelScheduler::new();
        enqueue_many(&scheduler, "urgent", 0, 50);
        enqueue_many(&scheduler, "batch", 100, 50);

        let served = dequeue_invocations(&scheduler, 30);
        let urgent = served.iter().filter(|i| *i == "urgent").count();
        assert_eq!((urgent, 30 - urgent), (20, 10), "{served:?}");
    }

    #[test]
    fn put_back_and_completion_stop_counting_an_action_as_running() {
        let scheduler = MultiLevelScheduler::new();
        enqueue_many(&scheduler, "a", 0, 2);
        enqueue_many(&scheduler, "b", 0, 2);

        let (first, sm) = scheduler.dequeue().unwrap();
        assert_eq!(*scheduler.running.lock().get("a").unwrap(), 1);
        scheduler.put_back(first, sm);
        assert!(scheduler.running.lock().is_empty());

        let (next, _) = scheduler.dequeue().unwrap();
        assert_eq!(next.command[0], "a", "put back, it keeps its turn");
        scheduler.complete_action(&next.action_digest);
        assert!(scheduler.running.lock().is_empty());
        scheduler.complete_action(&next.action_digest);
        assert!(
            scheduler.running.lock().is_empty(),
            "completing twice counts once"
        );
    }

    #[test]
    fn a_critical_action_is_dequeued_before_everything_queued_earlier() {
        let scheduler = MultiLevelScheduler::new();
        for i in 1..=20u32 {
            let d = digest(&format!("{i:064x}"), 1);
            let a = action(d, "echo");
            let sm = Arc::new(ExecutionStateMachine::new(a.operation_id, d));
            scheduler.enqueue(a, sm).unwrap();
        }
        let critical_digest = digest(&format!("{:064x}", 999u32), 1);
        let mut critical = action(critical_digest, "webpack");
        critical.priority = QueuePriority::Critical;
        let sm = Arc::new(ExecutionStateMachine::new(
            critical.operation_id,
            critical_digest,
        ));
        let critical_id = scheduler.enqueue(critical, sm).unwrap();

        let (first, _) = scheduler.dequeue().unwrap();
        assert_eq!(first.operation_id, critical_id);
        assert_eq!(scheduler.queued_len(), 20);
    }

    #[test]
    fn the_priority_property_picks_the_critical_queue_and_rejects_typos() {
        let props = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect()
        };
        assert_eq!(
            QueuePriority::from_platform(&props(&[("Priority", "critical")])),
            Ok(QueuePriority::Critical)
        );
        assert_eq!(
            QueuePriority::from_platform(&props(&[("OSFamily", "linux")])),
            Ok(QueuePriority::Medium)
        );
        assert!(QueuePriority::from_platform(&props(&[("Priority", "urgent")])).is_err());
    }

    #[test]
    fn enqueued_action_is_dequeued() {
        let scheduler = MultiLevelScheduler::new();
        let digest = digest(
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            1024,
        );
        let action = action(digest, "echo");
        let sm = Arc::new(ExecutionStateMachine::new(action.operation_id, digest));

        let op_id = scheduler.enqueue(action, sm).unwrap();
        assert!(scheduler.has_work());
        assert_eq!(scheduler.stats().medium_queued, 1);

        let (dequeued, _) = scheduler.dequeue().unwrap();
        assert_eq!(dequeued.operation_id, op_id);
    }

    #[test]
    fn identical_in_flight_actions_are_merged() {
        let scheduler = MultiLevelScheduler::new();
        let digest = digest(
            "54093d159648c4e0858b989afc5a8333aedf8e96e9c58d1f7b1d0f0b6147f4c1",
            2048,
        );
        let action1 = action(digest, "cmd1");
        let action2 = action(digest, "cmd2");
        let sm1 = Arc::new(ExecutionStateMachine::new(action1.operation_id, digest));
        let sm2 = Arc::new(ExecutionStateMachine::new(action2.operation_id, digest));

        let op_id1 = scheduler.enqueue(action1, sm1).unwrap();
        let op_id2 = scheduler.enqueue(action2, sm2).unwrap();

        assert_eq!(op_id1, op_id2);
        assert_eq!(scheduler.stats().medium_queued, 1);
    }

    #[test]
    fn concurrent_identical_actions_are_queued_once() {
        const CALLERS: usize = 16;
        let digest = digest(
            "54093d159648c4e0858b989afc5a8333aedf8e96e9c58d1f7b1d0f0b6147f4c1",
            2048,
        );
        for _ in 0..200 {
            let scheduler = Arc::new(MultiLevelScheduler::new());
            let barrier = Arc::new(std::sync::Barrier::new(CALLERS));
            let owners: Vec<OperationId> = (0..CALLERS)
                .map(|_| {
                    let (scheduler, barrier) = (scheduler.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        let action = action(digest, "cmd");
                        let sm = Arc::new(ExecutionStateMachine::new(action.operation_id, digest));
                        barrier.wait();
                        scheduler.enqueue(action, sm).unwrap()
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|caller| caller.join().unwrap())
                .collect();

            assert!(owners.iter().all(|owner| *owner == owners[0]), "{owners:?}");
            assert_eq!(scheduler.queued_len(), 1);
        }
    }
}

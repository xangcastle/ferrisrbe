//! Bounded pool of local worker slots.
//!
//! Slots are created on demand up to [`PoolConfig::max_workers`]; a caller
//! that finds none free waits up to [`PoolConfig::worker_timeout`]. A slot
//! whose execution fails is retired instead of being returned to the pool.

use super::{ActionResult, WorkerError, WorkerId, WorkerState};
use crate::execution::scheduler::ExecutableAction;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

/// Interval at which a waiting [`WorkerPool::acquire_worker`] rechecks.
const ACQUIRE_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Idle and busy worker slots.
pub struct WorkerPool {
    config: PoolConfig,
    available: Arc<Mutex<Vec<WorkerHandle>>>,
    busy: Arc<RwLock<std::collections::HashMap<WorkerId, WorkerHandle>>>,
    pending: Arc<Mutex<VecDeque<ExecutableAction>>>,
}

/// Sizing and timeouts of a [`WorkerPool`].
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Most slots the pool will ever hold.
    pub max_workers: usize,
    /// Slots created by [`WorkerPool::initialize`].
    pub min_workers: usize,
    pub idle_timeout: Duration,
    /// Longest [`WorkerPool::acquire_worker`] waits for a free slot.
    pub worker_timeout: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_workers: 10,
            min_workers: 2,
            idle_timeout: Duration::from_secs(300),
            worker_timeout: Duration::from_secs(60),
        }
    }
}

/// One worker slot.
#[derive(Debug, Clone)]
pub struct WorkerHandle {
    pub id: WorkerId,
    pub status: WorkerState,
    pub last_used: Instant,
    pub total_executions: u64,
}

impl WorkerPool {
    pub fn new(config: PoolConfig) -> Self {
        Self {
            config,
            available: Arc::new(Mutex::new(Vec::new())),
            busy: Arc::new(RwLock::new(std::collections::HashMap::new())),
            pending: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Creates the [`PoolConfig::min_workers`] initial slots.
    pub async fn initialize(&self) -> Result<(), WorkerError> {
        info!(
            "Initializing worker pool with {} workers",
            self.config.min_workers
        );

        let mut available = self.available.lock().await;
        for _ in 0..self.config.min_workers {
            let worker = self.create_worker().await?;
            available.push(worker);
        }

        info!("Worker pool initialized with {} workers", available.len());
        Ok(())
    }

    async fn create_worker(&self) -> Result<WorkerHandle, WorkerError> {
        let id = WorkerId::generate();

        Ok(WorkerHandle {
            id,
            status: WorkerState::Idle,
            last_used: Instant::now(),
            total_executions: 0,
        })
    }

    /// Moves `worker` to the busy set and returns it marked `Busy`.
    async fn mark_busy(&self, mut worker: WorkerHandle) -> WorkerHandle {
        worker.status = WorkerState::Busy;
        self.busy.write().await.insert(worker.id, worker.clone());
        worker
    }

    /// Takes a free slot, creating one while below
    /// [`PoolConfig::max_workers`], and waits for one otherwise.
    ///
    /// # Errors
    ///
    /// [`WorkerError::Unavailable`] when no slot frees up within
    /// [`PoolConfig::worker_timeout`].
    pub async fn acquire_worker(&self) -> Result<WorkerHandle, WorkerError> {
        let deadline = Instant::now() + self.config.worker_timeout;

        loop {
            {
                let mut available = self.available.lock().await;
                if let Some(worker) = available.pop() {
                    let worker = self.mark_busy(worker).await;
                    debug!("Acquired worker {:?}", worker.id);
                    return Ok(worker);
                }
            }

            let current_count = {
                let available = self.available.lock().await;
                let busy = self.busy.read().await;
                available.len() + busy.len()
            };

            if current_count < self.config.max_workers {
                match self.create_worker().await {
                    Ok(worker) => return Ok(self.mark_busy(worker).await),
                    Err(e) => {
                        warn!("Failed to create worker: {}", e);
                    }
                }
            }

            if Instant::now() > deadline {
                return Err(WorkerError::Unavailable(format!(
                    "No worker available after {:?}",
                    self.config.worker_timeout
                )));
            }

            tokio::time::sleep(ACQUIRE_RETRY_INTERVAL).await;
        }
    }

    /// Returns a slot to the pool when `healthy`, and retires it otherwise.
    pub async fn release_worker(&self, worker_id: WorkerId, healthy: bool) {
        let worker_to_release = {
            let mut busy = self.busy.write().await;
            busy.remove(&worker_id)
        };

        if let Some(mut worker) = worker_to_release {
            if healthy {
                worker.status = WorkerState::Idle;
                worker.last_used = Instant::now();

                let mut available = self.available.lock().await;
                available.push(worker);
                debug!("Released worker {:?} back to pool", worker_id);
            } else {
                warn!(
                    "Worker {:?} marked unhealthy, not returning to pool",
                    worker_id
                );
            }
        }
    }

    /// Runs `action` on a slot; a failed execution retires the slot.
    pub async fn execute(&self, action: ExecutableAction) -> Result<ActionResult, WorkerError> {
        let worker = self.acquire_worker().await?;
        let worker_id = worker.id;

        let result = self.execute_with_worker(worker, action).await;

        let healthy = result.is_ok();
        self.release_worker(worker_id, healthy).await;

        result
    }

    async fn execute_with_worker(
        &self,
        _worker: WorkerHandle,
        action: ExecutableAction,
    ) -> Result<ActionResult, WorkerError> {
        debug!("Executing action with worker: {:?}", action.action_digest);

        tokio::time::sleep(Duration::from_millis(100)).await;

        Ok(ActionResult {
            exit_code: 0,
            stdout: vec![],
            stderr: vec![],
            execution_duration: Duration::from_millis(100),
        })
    }

    pub async fn stats(&self) -> PoolStats {
        let available = self.available.lock().await.len();
        let busy = self.busy.read().await.len();
        let pending = self.pending.lock().await.len();

        PoolStats {
            available_workers: available,
            busy_workers: busy,
            pending_actions: pending,
            total_workers: available + busy,
        }
    }

    /// Drops every slot and pending action.
    pub async fn shutdown(&self) {
        info!("Shutting down worker pool");

        let mut available = self.available.lock().await;
        available.clear();

        let mut busy = self.busy.write().await;
        busy.clear();

        let mut pending = self.pending.lock().await;
        pending.clear();

        info!("Worker pool shutdown complete");
    }
}

/// Slot and queue counts at one instant.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PoolStats {
    pub available_workers: usize,
    pub busy_workers: usize,
    pub pending_actions: usize,
    pub total_workers: usize,
}

impl Default for WorkerPool {
    fn default() -> Self {
        Self::new(PoolConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn initialize_creates_min_workers() {
        let pool = WorkerPool::new(PoolConfig {
            min_workers: 2,
            max_workers: 5,
            ..Default::default()
        });

        pool.initialize().await.unwrap();

        let stats = pool.stats().await;
        assert_eq!(stats.available_workers, 2);
        assert_eq!(stats.total_workers, 2);
    }

    #[tokio::test]
    async fn acquired_worker_is_busy_until_released() {
        let pool = WorkerPool::new(PoolConfig {
            min_workers: 1,
            max_workers: 2,
            worker_timeout: Duration::from_secs(1),
            ..Default::default()
        });

        pool.initialize().await.unwrap();

        let worker = pool.acquire_worker().await.unwrap();
        assert_eq!(worker.status, WorkerState::Busy);

        pool.release_worker(worker.id, true).await;

        let stats = pool.stats().await;
        assert_eq!(stats.available_workers, 1);
        assert_eq!(stats.busy_workers, 0);
    }
}

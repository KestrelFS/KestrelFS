// SPDX-License-Identifier: Apache-2.0
//! Bounded asynchronous ObjectStore garbage deletion.
//!
//! The worker deliberately does not mutate MetaStore.  It only performs the
//! potentially slow remote deletes and returns their outcomes to the daemon's
//! single IPC thread.  That thread acknowledges successful keys in the durable
//! GC queue, preserving FileMetaStore's single-writer discipline.

use crate::object_store::{ObjectStore, ObjectStoreError};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::task::{Id, JoinSet};

pub(crate) const GC_DELETE_CONCURRENCY: usize = 4;
pub(crate) const GC_REQUEST_QUEUE_CAPACITY: usize = 32;
pub(crate) const GC_REQUEST_MAX_KEYS: usize = 64;

#[derive(Debug)]
struct DeleteRequest {
    operation: String,
    keys: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct DeleteCompletion {
    pub(crate) operation: String,
    pub(crate) succeeded: Vec<String>,
    pub(crate) failed: Vec<(String, ObjectStoreError)>,
}

impl DeleteCompletion {
    pub(crate) fn attempted(&self) -> usize {
        self.succeeded.len() + self.failed.len()
    }

    fn all_keys(&self) -> impl Iterator<Item = &String> {
        self.succeeded
            .iter()
            .chain(self.failed.iter().map(|(key, _)| key))
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ScheduleOutcome {
    pub(crate) queued: usize,
    pub(crate) already_in_flight: usize,
    pub(crate) backpressured: usize,
}

#[derive(Clone)]
pub(crate) struct GcScheduler {
    requests: mpsc::Sender<DeleteRequest>,
    in_flight: Arc<Mutex<HashSet<String>>>,
}

impl GcScheduler {
    fn new(requests: mpsc::Sender<DeleteRequest>) -> Self {
        Self {
            requests,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Non-blockingly schedules keys. Full queues are safe: rejected keys are
    /// removed from the in-flight set and remain in MetaStore's durable queue.
    pub(crate) fn schedule(&self, operation: &str, keys: Vec<String>) -> ScheduleOutcome {
        let mut unique = HashSet::new();
        let mut queued_keys = Vec::new();
        let mut already_in_flight = 0;
        {
            let mut in_flight = self.in_flight.lock().unwrap();
            for key in keys {
                if !unique.insert(key.clone()) || !in_flight.insert(key.clone()) {
                    already_in_flight += 1;
                } else {
                    queued_keys.push(key);
                }
            }
        }

        if queued_keys.is_empty() {
            return ScheduleOutcome {
                already_in_flight,
                ..ScheduleOutcome::default()
            };
        }

        let mut queued = 0;
        let mut backpressured = 0;
        for keys in queued_keys
            .chunks(GC_REQUEST_MAX_KEYS)
            .map(<[String]>::to_vec)
        {
            let key_count = keys.len();
            match self.requests.try_send(DeleteRequest {
                operation: operation.to_string(),
                keys,
            }) {
                Ok(()) => queued += key_count,
                Err(error) => {
                    let request = error.into_inner();
                    let mut in_flight = self.in_flight.lock().unwrap();
                    for key in &request.keys {
                        in_flight.remove(key);
                    }
                    backpressured += request.keys.len();
                }
            }
        }
        ScheduleOutcome {
            queued,
            already_in_flight,
            backpressured,
        }
    }

    pub(crate) fn complete(&self, completion: &DeleteCompletion) {
        let mut in_flight = self.in_flight.lock().unwrap();
        for key in completion.all_keys() {
            in_flight.remove(key);
        }
    }
}

pub(crate) struct GcWorker {
    scheduler: GcScheduler,
    completions: mpsc::Receiver<DeleteCompletion>,
}

impl GcWorker {
    pub(crate) fn start(
        handle: &tokio::runtime::Handle,
        object_store: Arc<dyn ObjectStore>,
    ) -> Self {
        let (request_tx, request_rx) = mpsc::channel(GC_REQUEST_QUEUE_CAPACITY);
        let (completion_tx, completion_rx) = mpsc::channel(GC_REQUEST_QUEUE_CAPACITY);
        let scheduler = GcScheduler::new(request_tx);
        handle.spawn(run_worker(request_rx, completion_tx, object_store));
        Self {
            scheduler,
            completions: completion_rx,
        }
    }

    pub(crate) fn scheduler(&self) -> GcScheduler {
        self.scheduler.clone()
    }

    pub(crate) fn try_recv(&mut self) -> Option<DeleteCompletion> {
        self.completions.try_recv().ok()
    }
}

async fn run_worker(
    mut requests: mpsc::Receiver<DeleteRequest>,
    completions: mpsc::Sender<DeleteCompletion>,
    object_store: Arc<dyn ObjectStore>,
) {
    while let Some(request) = requests.recv().await {
        let completion = delete_bounded(request, &object_store).await;
        if completions.send(completion).await.is_err() {
            break;
        }
    }
}

async fn delete_bounded(
    request: DeleteRequest,
    object_store: &Arc<dyn ObjectStore>,
) -> DeleteCompletion {
    let mut remaining = request.keys.into_iter();
    let mut tasks = JoinSet::new();
    let mut task_keys: HashMap<Id, String> = HashMap::new();
    let mut succeeded = Vec::new();
    let mut failed = Vec::new();

    for _ in 0..GC_DELETE_CONCURRENCY {
        spawn_next_delete(&mut remaining, &mut tasks, &mut task_keys, object_store);
    }

    while let Some(joined) = tasks.join_next_with_id().await {
        match joined {
            Ok((id, result)) => {
                let key = task_keys
                    .remove(&id)
                    .expect("GC delete task must retain its key");
                match result {
                    Ok(()) => succeeded.push(key),
                    Err(error) => failed.push((key, error)),
                }
            }
            Err(error) => {
                let key = task_keys
                    .remove(&error.id())
                    .expect("failed GC delete task must retain its key");
                failed.push((
                    key,
                    ObjectStoreError::Io(format!("GC delete task failed: {error}")),
                ));
            }
        }
        spawn_next_delete(&mut remaining, &mut tasks, &mut task_keys, object_store);
    }

    DeleteCompletion {
        operation: request.operation,
        succeeded,
        failed,
    }
}

fn spawn_next_delete(
    remaining: &mut impl Iterator<Item = String>,
    tasks: &mut JoinSet<crate::object_store::Result<()>>,
    task_keys: &mut HashMap<Id, String>,
    object_store: &Arc<dyn ObjectStore>,
) {
    let Some(key) = remaining.next() else {
        return;
    };
    let task_key = key.clone();
    let object_store = Arc::clone(object_store);
    let handle = tasks.spawn(async move { object_store.delete(&task_key).await });
    task_keys.insert(handle.id(), key);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::{MemObjectStore, Result};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[derive(Clone)]
    struct MeasuredStore {
        inner: MemObjectStore,
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ObjectStore for MeasuredStore {
        async fn get(&self, key: &str) -> Result<Vec<u8>> {
            self.inner.get(key).await
        }

        async fn put(&self, key: String, value: Vec<u8>) -> Result<()> {
            self.inner.put(key, value).await
        }

        async fn delete(&self, key: &str) -> Result<()> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            if key == "fail" {
                Err(ObjectStoreError::Io("injected failure".to_string()))
            } else {
                self.inner.delete(key).await
            }
        }
    }

    #[tokio::test]
    async fn bounded_queue_reports_backpressure_and_releases_rejected_key() {
        let (sender, mut receiver) = mpsc::channel(1);
        let scheduler = GcScheduler::new(sender);
        assert_eq!(scheduler.schedule("one", vec!["a".to_string()]).queued, 1);
        assert_eq!(
            scheduler.schedule("two", vec!["b".to_string()]),
            ScheduleOutcome {
                backpressured: 1,
                ..ScheduleOutcome::default()
            }
        );
        receiver.recv().await.unwrap();
        assert_eq!(scheduler.schedule("retry", vec!["b".to_string()]).queued, 1);
    }

    #[tokio::test]
    async fn one_request_cannot_bypass_the_bounded_key_budget() {
        let (sender, mut receiver) = mpsc::channel(1);
        let scheduler = GcScheduler::new(sender);
        let keys: Vec<_> = (0..=GC_REQUEST_MAX_KEYS)
            .map(|index| format!("key-{index}"))
            .collect();
        assert_eq!(
            scheduler.schedule("large", keys),
            ScheduleOutcome {
                queued: GC_REQUEST_MAX_KEYS,
                already_in_flight: 0,
                backpressured: 1,
            }
        );
        receiver.recv().await.unwrap();
        assert_eq!(
            scheduler
                .schedule("retry", vec![format!("key-{GC_REQUEST_MAX_KEYS}")])
                .queued,
            1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worker_bounds_concurrency_and_reports_delete_failures() {
        let store = MeasuredStore {
            inner: MemObjectStore::new(),
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        };
        let peak = Arc::clone(&store.peak);
        let object_store: Arc<dyn ObjectStore> = Arc::new(store);
        let mut worker = GcWorker::start(&tokio::runtime::Handle::current(), object_store);
        let scheduler = worker.scheduler();
        let mut keys: Vec<_> = (0..11).map(|index| format!("key-{index}")).collect();
        keys.push("fail".to_string());
        assert_eq!(scheduler.schedule("test", keys).queued, 12);

        let completion = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(completion) = worker.try_recv() {
                    break completion;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(completion.succeeded.len(), 11);
        assert_eq!(completion.failed.len(), 1);
        assert_eq!(completion.failed[0].0, "fail");
        assert!((2..=GC_DELETE_CONCURRENCY).contains(&peak.load(Ordering::SeqCst)));
        scheduler.complete(&completion);
        assert_eq!(
            scheduler.schedule("retry", vec!["fail".to_string()]).queued,
            1
        );
    }
}

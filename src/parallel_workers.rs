use std::future::Future;
use std::num::NonZeroUsize;

use anyhow::{Context, Result};

/// Универсальная ошибка bucket-worker с привязкой к исходной задаче.
pub(crate) struct WorkerFailure<TTask> {
    pub(crate) task: TTask,
    pub(crate) error: anyhow::Error,
}

/// Универсальный результат bucket-worker:
/// завершённые задачи плюс первая фатальная ошибка (если была).
pub(crate) struct WorkerOutcome<TTask, TResult> {
    pub(crate) completed: Vec<(usize, TResult)>,
    pub(crate) failure: Option<WorkerFailure<TTask>>,
}

impl<TTask, TResult> WorkerOutcome<TTask, TResult> {
    pub(crate) const fn empty() -> Self {
        Self::success(Vec::new())
    }

    pub(crate) const fn success(completed: Vec<(usize, TResult)>) -> Self {
        Self {
            completed,
            failure: None,
        }
    }

    pub(crate) const fn with_failure(
        completed: Vec<(usize, TResult)>,
        task: TTask,
        error: anyhow::Error,
    ) -> Self {
        Self {
            completed,
            failure: Some(WorkerFailure { task, error }),
        }
    }
}

fn bucketize_indexed<T: Clone>(items: &[T], concurrency: NonZeroUsize) -> Vec<Vec<(usize, T)>> {
    let workers_count = concurrency.get().min(items.len());
    if workers_count == 0 {
        return Vec::new();
    }

    let mut buckets = vec![Vec::new(); workers_count];
    for (index, item) in items.iter().cloned().enumerate() {
        buckets[index % workers_count].push((index, item));
    }
    buckets
}

/// Распределяет задачи по bucket-ам и запускает worker future для каждого bucket-а.
pub(crate) fn spawn_bucket_workers<T, O, F, Fut>(
    items: &[T],
    concurrency: NonZeroUsize,
    mut spawn_worker: F,
) -> tokio::task::JoinSet<O>
where
    T: Clone + Send + 'static,
    O: Send + 'static,
    F: FnMut(Vec<(usize, T)>) -> Fut,
    Fut: Future<Output = O> + Send + 'static,
{
    let mut workers = tokio::task::JoinSet::new();
    for tasks in bucketize_indexed(items, concurrency) {
        workers.spawn(spawn_worker(tasks));
    }
    workers
}

/// Дожидается завершения worker-ов; при ошибке обработчика outcome прерывает оставшиеся задачи.
pub(crate) async fn process_joinset_outcomes<O, F>(
    workers: &mut tokio::task::JoinSet<O>,
    join_error_context: &'static str,
    mut on_outcome: F,
) -> Result<()>
where
    O: Send + 'static,
    F: FnMut(O) -> Result<()>,
{
    while let Some(join_result) = workers.join_next().await {
        if let Err(error) = join_result
            .context(join_error_context)
            .and_then(&mut on_outcome)
        {
            abort_and_drain(workers).await;
            return Err(error);
        }
    }
    Ok(())
}

pub(crate) async fn abort_and_drain<T: 'static>(workers: &mut tokio::task::JoinSet<T>) {
    workers.abort_all();
    while workers.join_next().await.is_some() {}
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::{bucketize_indexed, process_joinset_outcomes};

    #[test]
    fn assigns_each_index_to_one_nonempty_bucket_in_order() {
        assert_eq!(
            bucketize_indexed(&[10, 20, 30, 40, 50], NonZeroUsize::new(2).unwrap()),
            vec![vec![(0, 10), (2, 30), (4, 50)], vec![(1, 20), (3, 40)]]
        );
        assert_eq!(
            bucketize_indexed(&[10, 20], NonZeroUsize::new(3).unwrap()),
            vec![vec![(0, 10)], vec![(1, 20)]]
        );
        assert_eq!(
            bucketize_indexed::<u8>(&[], NonZeroUsize::MIN),
            [] as [Vec<(usize, u8)>; 0]
        );
    }

    #[tokio::test]
    async fn cancels_and_drains_workers_on_join_or_outcome_error() {
        struct DropSignal(Arc<AtomicBool>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        for join_failure in [false, true] {
            let dropped = Arc::new(AtomicBool::new(false));
            let signal = DropSignal(dropped.clone());
            let mut workers = tokio::task::JoinSet::new();
            workers.spawn(async move {
                let _signal = signal;
                std::future::pending::<()>().await;
            });
            workers.spawn(async move {
                assert!(!join_failure, "worker panicked");
            });

            let error = process_joinset_outcomes(&mut workers, "join failed", |()| {
                anyhow::bail!("operation failed")
            })
            .await
            .expect_err("worker failure must propagate");

            assert_eq!(
                error.to_string(),
                if join_failure {
                    "join failed"
                } else {
                    "operation failed"
                }
            );
            assert!(workers.is_empty());
            assert!(dropped.load(Ordering::SeqCst));
        }
    }
}

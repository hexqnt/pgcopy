use anyhow::{Context, Result};
use std::future::Future;

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
    pub(crate) fn empty() -> Self {
        Self {
            completed: Vec::new(),
            failure: None,
        }
    }

    pub(crate) fn success(completed: Vec<(usize, TResult)>) -> Self {
        Self {
            completed,
            failure: None,
        }
    }

    pub(crate) fn with_failure(
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

pub(crate) fn bucketize_indexed<T: Clone>(items: &[T], concurrency: usize) -> Vec<Vec<(usize, T)>> {
    let workers_count = concurrency.min(items.len());
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
    concurrency: usize,
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
        let outcome = join_result.context(join_error_context)?;
        if let Err(error) = on_outcome(outcome) {
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

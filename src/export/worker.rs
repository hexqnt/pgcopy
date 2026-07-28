use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::ObjectConfig;
use crate::manifest::ManifestObject;
use crate::parallel_workers::WorkerOutcome;
use crate::pg;
use crate::types::DataFormat;

use super::object::export_object;

/// Результат worker-а экспорта:
/// completed содержит готовые manifest-объекты и время обработки; failure — первая фатальная ошибка.
pub type ExportWorkerOutcome = WorkerOutcome<ObjectConfig, ExportObjectResult>;

/// Результат экспорта одного объекта внутри worker-а.
pub struct ExportObjectResult {
    pub manifest_object: ManifestObject,
    pub elapsed: Duration,
}

/// Выполняет экспорт выделенного набора объектов одним worker-подключением.
pub async fn export_worker(
    source_config: &tokio_postgres::Config,
    scratch_dir: &Path,
    tasks: Vec<(usize, ObjectConfig)>,
    data_format: DataFormat,
    snapshot_id: Option<&str>,
) -> ExportWorkerOutcome {
    let Some((_, first_object)) = tasks.first().cloned() else {
        return ExportWorkerOutcome::empty();
    };

    let mut completed = Vec::with_capacity(tasks.len());
    let mut last_object = Some(first_object.clone());

    let client = match pg::connect(source_config).await {
        Ok(client) => client,
        Err(error) => {
            return ExportWorkerOutcome::with_failure(
                completed,
                first_object,
                error.context("failed to connect source database for parallel export"),
            );
        }
    };

    if let Some(snapshot_id) = snapshot_id {
        // Для consistent snapshot каждый worker открывает свою read-only tx
        // и привязывается к snapshot, экспортированному координатором.
        if let Err(error) = client
            .batch_execute("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .await
        {
            return ExportWorkerOutcome::with_failure(
                completed,
                first_object,
                anyhow::Error::new(error)
                    .context("failed to begin parallel export snapshot transaction"),
            );
        }

        if let Err(error) = set_transaction_snapshot(&client, snapshot_id).await {
            let _ = client.batch_execute("ROLLBACK").await;
            return ExportWorkerOutcome::with_failure(
                completed,
                first_object,
                error.context("failed to set parallel export snapshot"),
            );
        }
    }

    for (index, object) in tasks {
        last_object = Some(object.clone());
        let started_at = Instant::now();
        match export_object(&client, scratch_dir, index, &object, data_format)
            .await
            .with_context(|| format!("export object {} failed", object.source_label()))
        {
            Ok(manifest_object) => completed.push((
                index,
                ExportObjectResult {
                    manifest_object,
                    elapsed: started_at.elapsed(),
                },
            )),
            Err(error) => {
                if snapshot_id.is_some() {
                    let _ = client.batch_execute("ROLLBACK").await;
                }

                return ExportWorkerOutcome::with_failure(completed, object, error);
            }
        }
    }

    if snapshot_id.is_some()
        && let Err(error) = client.batch_execute("COMMIT").await
    {
        let object = last_object.expect("last task must exist for non-empty worker");
        return ExportWorkerOutcome::with_failure(
            completed,
            object,
            anyhow::Error::new(error)
                .context("failed to commit parallel export snapshot transaction"),
        );
    }

    ExportWorkerOutcome::success(completed)
}

async fn set_transaction_snapshot(
    client: &tokio_postgres::Client,
    snapshot_id: &str,
) -> Result<()> {
    // Snapshot id приходит из Postgres и может содержать `'`, поэтому
    // экранируем его перед вставкой в SQL.
    let escaped_snapshot = snapshot_id.replace('\'', "''");
    let sql = format!("SET TRANSACTION SNAPSHOT '{escaped_snapshot}'");
    client
        .batch_execute(&sql)
        .await
        .context("failed to set transaction snapshot")
}

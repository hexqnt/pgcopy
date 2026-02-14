use std::path::Path;

use anyhow::{Context, Result};

use crate::config::ObjectConfig;
use crate::manifest::ManifestObject;
use crate::pg;
use crate::types::DataFormat;

use super::object::export_object;

/// Ошибка worker-а экспорта с привязкой к объекту.
pub struct ExportWorkerFailure {
    pub index: usize,
    pub object: ObjectConfig,
    pub error: anyhow::Error,
}

/// Результат выполнения worker-а:
/// успешно обработанные объекты плюс первая фатальная ошибка (если была).
pub struct ExportWorkerOutcome {
    pub completed: Vec<(usize, ManifestObject)>,
    pub failure: Option<ExportWorkerFailure>,
}

/// Выполняет экспорт выделенного набора объектов одним worker-подключением.
pub async fn export_worker(
    source_config: &tokio_postgres::Config,
    scratch_dir: &Path,
    tasks: Vec<(usize, ObjectConfig)>,
    data_format: DataFormat,
    snapshot_id: Option<&str>,
) -> ExportWorkerOutcome {
    let Some((first_index, first_object)) = tasks.first().cloned() else {
        return ExportWorkerOutcome {
            completed: Vec::new(),
            failure: None,
        };
    };

    let mut completed = Vec::with_capacity(tasks.len());
    let mut last_task = Some((first_index, first_object.clone()));

    let client = match pg::connect(source_config).await {
        Ok(client) => client,
        Err(error) => {
            return ExportWorkerOutcome {
                completed,
                failure: Some(ExportWorkerFailure {
                    index: first_index,
                    object: first_object,
                    error: error.context("failed to connect source database for parallel export"),
                }),
            };
        }
    };

    if let Some(snapshot_id) = snapshot_id {
        // Для consistent snapshot каждый worker открывает свою read-only tx
        // и привязывается к snapshot, экспортированному координатором.
        if let Err(error) = client
            .batch_execute("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .await
        {
            return ExportWorkerOutcome {
                completed,
                failure: Some(ExportWorkerFailure {
                    index: first_index,
                    object: first_object,
                    error: anyhow::Error::new(error)
                        .context("failed to begin parallel export snapshot transaction"),
                }),
            };
        }

        if let Err(error) = set_transaction_snapshot(&client, snapshot_id).await {
            let _ = client.batch_execute("ROLLBACK").await;
            return ExportWorkerOutcome {
                completed,
                failure: Some(ExportWorkerFailure {
                    index: first_index,
                    object: first_object,
                    error: error.context("failed to set parallel export snapshot"),
                }),
            };
        }
    }

    for (index, object) in tasks {
        last_task = Some((index, object.clone()));
        match export_object(&client, scratch_dir, index, &object, data_format)
            .await
            .with_context(|| {
                format!(
                    "export object {}.{} failed",
                    object.select.source_schema, object.select.source_name
                )
            }) {
            Ok(manifest_object) => completed.push((index, manifest_object)),
            Err(error) => {
                if snapshot_id.is_some() {
                    let _ = client.batch_execute("ROLLBACK").await;
                }

                return ExportWorkerOutcome {
                    completed,
                    failure: Some(ExportWorkerFailure {
                        index,
                        object,
                        error,
                    }),
                };
            }
        }
    }

    if snapshot_id.is_some()
        && let Err(error) = client.batch_execute("COMMIT").await
    {
        let (index, object) = last_task.expect("last task must exist for non-empty worker");
        return ExportWorkerOutcome {
            completed,
            failure: Some(ExportWorkerFailure {
                index,
                object,
                error: anyhow::Error::new(error)
                    .context("failed to commit parallel export snapshot transaction"),
            }),
        };
    }

    ExportWorkerOutcome {
        completed,
        failure: None,
    }
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

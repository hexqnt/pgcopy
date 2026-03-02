use std::path::Path;

use anyhow::{Context, Result};

use crate::manifest::{Manifest, ManifestObject};
use crate::parallel_workers;
use crate::types::DataFormat;

use super::{ImportMode, copy_stream, load, progress::ImportProgress};

/// Ошибка worker-а импорта с привязкой к объекту.
struct ImportWorkerFailure {
    object: ManifestObject,
    error: anyhow::Error,
}

/// Результат выполнения worker-а:
/// успешно обработанные объекты плюс первая фатальная ошибка (если была).
struct ImportWorkerOutcome {
    completed: Vec<(usize, u64)>,
    failure: Option<ImportWorkerFailure>,
}

/// Импортирует объекты из распакованного bundle параллельно по bucket-ам.
pub async fn import_objects_parallel(
    target_config: &tokio_postgres::Config,
    scratch_dir: &Path,
    manifest: &Manifest,
    mode: ImportMode,
    concurrency: usize,
    ddl_only: bool,
    progress_enabled: bool,
) -> Result<u64> {
    let data_format = manifest.data_format;
    let progress = ImportProgress::new(manifest, progress_enabled);

    for object in &manifest.objects {
        progress.set_object_running(object);
    }
    let mut workers =
        parallel_workers::spawn_bucket_workers(&manifest.objects, concurrency, |tasks| {
            let target_config = target_config.clone();
            let scratch_dir = scratch_dir.to_path_buf();
            // Каждый worker использует отдельное соединение с target БД.
            async move {
                import_worker(
                    &target_config,
                    &scratch_dir,
                    tasks,
                    mode,
                    data_format,
                    ddl_only,
                )
                .await
            }
        });

    let mut total_rows = 0_u64;
    let workers_result = parallel_workers::process_joinset_outcomes(
        &mut workers,
        "parallel import worker task failed",
        |outcome| {
            for (index, inserted_rows) in outcome.completed {
                total_rows += inserted_rows;
                progress.set_object_done(&manifest.objects[index], inserted_rows);
            }

            if let Some(failure) = outcome.failure {
                progress.set_object_error(&failure.object, failure.error.as_ref());
                return Err(failure.error);
            }

            Ok(())
        },
    )
    .await;
    if let Err(error) = workers_result {
        progress.finish_with_error(error.as_ref());
        return Err(error);
    }

    progress.finish_done(total_rows);
    Ok(total_rows)
}

async fn import_worker(
    target_config: &tokio_postgres::Config,
    scratch_dir: &Path,
    tasks: Vec<(usize, ManifestObject)>,
    mode: ImportMode,
    data_format: DataFormat,
    ddl_only: bool,
) -> ImportWorkerOutcome {
    let Some((_, first_object)) = tasks.first().cloned() else {
        return ImportWorkerOutcome {
            completed: Vec::new(),
            failure: None,
        };
    };

    let mut completed = Vec::with_capacity(tasks.len());

    let client = crate::pg::connect(target_config)
        .await
        .with_context(|| "failed to connect target database for parallel import worker".to_owned());
    let client = match client {
        Ok(client) => client,
        Err(error) => {
            return ImportWorkerOutcome {
                completed,
                failure: Some(ImportWorkerFailure {
                    object: first_object,
                    error,
                }),
            };
        }
    };

    for (index, object) in tasks {
        let imported = import_object(&client, scratch_dir, &object, mode, data_format, ddl_only)
            .await
            .with_context(|| {
                format!(
                    "manifest object index {} ({}.{})",
                    index + 1,
                    object.target_schema,
                    object.target_name
                )
            });
        match imported {
            Ok(inserted_rows) => completed.push((index, inserted_rows)),
            Err(error) => {
                return ImportWorkerOutcome {
                    completed,
                    failure: Some(ImportWorkerFailure { object, error }),
                };
            }
        }
    }

    ImportWorkerOutcome {
        completed,
        failure: None,
    }
}

async fn import_object(
    client: &tokio_postgres::Client,
    scratch_dir: &Path,
    object: &ManifestObject,
    mode: ImportMode,
    data_format: DataFormat,
    ddl_only: bool,
) -> Result<u64> {
    let ddl_sql = read_ddl_from_bundle(scratch_dir, &object.ddl_path).await?;
    if ddl_only {
        load::prepare_object_ddl_only(client, object, mode, &ddl_sql).await?;
        return Ok(0);
    }

    let data_path = scratch_dir.join(&object.data_path);
    let inserted_rows = load::load_object(client, object, mode, &ddl_sql, || async {
        copy_stream::copy_data_in_file(
            client,
            &data_path,
            &object.target_schema,
            &object.target_name,
            &object.effective_columns,
            data_format,
        )
        .await
        .with_context(|| {
            format!(
                "failed to import data into {}.{} from {}",
                object.target_schema,
                object.target_name,
                data_path.display()
            )
        })
    })
    .await?;

    Ok(inserted_rows)
}

async fn read_ddl_from_bundle(scratch_dir: &Path, ddl_rel_path: &str) -> Result<String> {
    let ddl_path = scratch_dir.join(ddl_rel_path);
    tokio::fs::read_to_string(&ddl_path)
        .await
        .with_context(|| format!("failed to read DDL file {}", ddl_path.display()))
}

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::manifest::{Manifest, ManifestObject};
use crate::parallel_workers::{self, WorkerOutcome};
use crate::types::DataFormat;

use super::{ImportMode, copy_stream, load, progress::ImportProgress};

/// Результат worker-а импорта:
/// completed содержит количество загруженных строк по объектам и время обработки.
type ImportWorkerOutcome = WorkerOutcome<ManifestObject, ImportObjectResult>;

/// Результат импорта одного объекта внутри worker-а.
struct ImportObjectResult {
    inserted_rows: u64,
    elapsed: Duration,
}

pub(super) struct ImportParallelOptions {
    pub(super) mode: ImportMode,
    pub(super) concurrency: usize,
    pub(super) ddl_only: bool,
    pub(super) operation_started_at: Instant,
    pub(super) progress_enabled: bool,
}

/// Импортирует объекты из распакованного bundle параллельно по bucket-ам.
pub async fn import_objects_parallel(
    target_config: &tokio_postgres::Config,
    scratch_dir: &Path,
    manifest: &Manifest,
    options: ImportParallelOptions,
) -> Result<u64> {
    let ImportParallelOptions {
        mode,
        concurrency,
        ddl_only,
        operation_started_at,
        progress_enabled,
    } = options;
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
            for (index, result) in outcome.completed {
                total_rows += result.inserted_rows;
                progress.set_object_done(
                    &manifest.objects[index],
                    result.inserted_rows,
                    result.elapsed,
                );
            }

            if let Some(failure) = outcome.failure {
                progress.set_object_error(&failure.task, failure.error.as_ref());
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

    progress.finish_done(total_rows, operation_started_at.elapsed());
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
        return ImportWorkerOutcome::empty();
    };

    let mut completed = Vec::with_capacity(tasks.len());

    let client = crate::pg::connect(target_config)
        .await
        .with_context(|| "failed to connect target database for parallel import worker".to_owned());
    let client = match client {
        Ok(client) => client,
        Err(error) => return ImportWorkerOutcome::with_failure(completed, first_object, error),
    };

    for (index, object) in tasks {
        let object_started_at = Instant::now();
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
            Ok(inserted_rows) => completed.push((
                index,
                ImportObjectResult {
                    inserted_rows,
                    elapsed: object_started_at.elapsed(),
                },
            )),
            Err(error) => return ImportWorkerOutcome::with_failure(completed, object, error),
        }
    }

    ImportWorkerOutcome::success(completed)
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
    if !object.requires_data_load() {
        load::prepare_object_ddl_only(client, object, mode, &ddl_sql).await?;
        return Ok(0);
    }

    let data_path = scratch_dir.join(&object.data_path);
    let inserted_rows = load::load_object(client, object, mode, &ddl_sql, || async {
        let mut source = copy_stream::FileChunkSource::open(&data_path).await?;
        copy_stream::copy_data_in(
            client,
            &mut source,
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

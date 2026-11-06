mod object;
mod plan;
mod progress;
mod session;
mod worker;

use std::env;
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::bundle_io;
use crate::config::{Config, GeneralConfig, ObjectConfig};
use crate::crypto;
use crate::manifest::{Manifest, ManifestObject};
use crate::parallel_workers;
use crate::pg;

use object::validate_target_collisions;
use plan::build_export_plan;
use progress::ExportProgress;
use session::{ExportContext, ExportSession, run_with_snapshot_support};
use worker::export_worker;

pub(crate) fn resolve_export_concurrency(
    cli_concurrency: Option<NonZeroUsize>,
    general: &GeneralConfig,
) -> Result<NonZeroUsize> {
    if let Some(concurrency) = cli_concurrency {
        return Ok(concurrency);
    }

    if general.concurrency_from_toml {
        return Ok(general.concurrency);
    }

    // Приоритет параметров: CLI > TOML > ENV > fallback.
    let env_name = "PGCOPY_CONCURRENCY";
    match env::var(env_name) {
        Ok(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(general.concurrency);
            }

            let parsed = trimmed.parse::<NonZeroUsize>().map_err(|_| {
                anyhow::anyhow!("invalid {env_name} value '{trimmed}', expected integer >= 1")
            })?;
            Ok(parsed)
        }
        Err(env::VarError::NotPresent) => Ok(general.concurrency),
        Err(env::VarError::NotUnicode(_)) => {
            bail!("environment variable {env_name} contains non-Unicode data")
        }
    }
}

/// Выполняет экспорт объектов в bundle.
pub async fn run(
    config: Config,
    out_path: &Path,
    cli_concurrency: Option<NonZeroUsize>,
    bundle_password: Option<&str>,
    source_config: tokio_postgres::Config,
    progress_enabled: bool,
) -> Result<()> {
    let operation_started_at = Instant::now();
    let concurrency = resolve_export_concurrency(cli_concurrency, &config.general)?;
    let password = crypto::resolve_bundle_password(bundle_password)?;
    let client = pg::connect(&source_config).await?;
    let export_objects_plan = build_export_plan(&client, config.objects).await?;
    let progress = ExportProgress::new(export_objects_plan.len(), progress_enabled);

    let source_pg_version_num = pg::server_version_num(&client).await?;
    let source_fingerprint = Some(pg::source_fingerprint(&client).await?);

    let scratch = tempfile::tempdir().context("failed to create temporary directory for export")?;
    std::fs::create_dir_all(scratch.path().join("ddl"))?;
    std::fs::create_dir_all(scratch.path().join("data"))?;

    let context = ExportContext {
        source_config: &source_config,
        scratch_dir: scratch.path(),
        data_format: config.general.data_format,
        snapshot_id: None,
        retry: config.general.retry,
    };
    let export_plan = &export_objects_plan;
    let export_progress = &progress;
    let export_result = run_with_snapshot_support(
        &client,
        config.general.consistent_snapshot,
        |snapshot_id| async move {
            let context = ExportContext {
                snapshot_id: snapshot_id.as_deref(),
                ..context
            };
            let objects = if concurrency.get() == 1 {
                export_objects(context, export_plan, export_progress).await?
            } else {
                export_objects_parallel(context, export_plan, export_progress, concurrency).await?
            };
            validate_target_collisions(&objects)?;
            Ok(objects)
        },
    )
    .await;

    let manifest_objects = match export_result {
        Ok(manifest_objects) => manifest_objects,
        Err(error) => {
            progress.finish_with_error(error.as_ref());
            return Err(error);
        }
    };

    let manifest = Manifest {
        format_version: 2,
        created_at: Utc::now().to_rfc3339(),
        source_fingerprint,
        source_pg_version_num,
        data_format: config.general.data_format,
        consistent_snapshot: config.general.consistent_snapshot,
        objects: manifest_objects,
    };

    progress.set_bundle_running();
    let bundle_scratch_path = scratch.path().to_path_buf();
    let bundle_out_path = out_path.to_path_buf();
    let bundle_password = password;
    let bundle_manifest = manifest;
    let write_result = tokio::task::spawn_blocking(move || {
        bundle_io::write_bundle(
            &bundle_scratch_path,
            &bundle_out_path,
            &bundle_manifest,
            bundle_password.as_ref(),
        )
    })
    .await
    .context("bundle writer task failed")?;
    match write_result {
        Ok(()) => {
            progress.finish_bundle_done(out_path, operation_started_at.elapsed());
            Ok(())
        }
        Err(error) => {
            progress.finish_bundle_error(out_path, error.as_ref());
            Err(error)
        }
    }
}

async fn export_objects(
    context: ExportContext<'_>,
    objects: &[ObjectConfig],
    progress: &ExportProgress,
) -> Result<Vec<ManifestObject>> {
    let mut session = ExportSession::new(context);
    let mut manifest_objects = Vec::with_capacity(objects.len());

    for (index, object) in objects.iter().enumerate() {
        progress.set_object_running(object);
        let object_started_at = Instant::now();
        let manifest_object = session.export(index, object).await;

        match manifest_object {
            Ok(manifest_object) => {
                progress.set_object_done(&manifest_object, object_started_at.elapsed());
                manifest_objects.push(manifest_object);
            }
            Err(error) => {
                progress.set_object_error(object, error.as_ref());
                return Err(error);
            }
        }
    }

    Ok(manifest_objects)
}

async fn export_objects_parallel(
    context: ExportContext<'_>,
    objects: &[ObjectConfig],
    progress: &ExportProgress,
    concurrency: NonZeroUsize,
) -> Result<Vec<ManifestObject>> {
    let mut ordered_objects = vec![None; objects.len()];
    for object in objects {
        progress.set_object_running(object);
    }
    let data_format = context.data_format;
    let retry = context.retry;
    let mut workers = parallel_workers::spawn_bucket_workers(objects, concurrency, |tasks| {
        let source_config = context.source_config.clone();
        let scratch_dir = context.scratch_dir.to_path_buf();
        let snapshot_id = context.snapshot_id.map(str::to_owned);
        async move {
            export_worker(
                ExportContext {
                    source_config: &source_config,
                    scratch_dir: &scratch_dir,
                    snapshot_id: snapshot_id.as_deref(),
                    data_format,
                    retry,
                },
                tasks,
            )
            .await
        }
    });

    parallel_workers::process_joinset_outcomes(
        &mut workers,
        "parallel export worker task failed",
        |outcome| {
            for (index, result) in outcome.completed {
                progress.set_object_done(&result.manifest_object, result.elapsed);
                ordered_objects[index] = Some(result.manifest_object);
            }

            if let Some(failure) = outcome.failure {
                progress.set_object_error(&failure.task, failure.error.as_ref());
                return Err(failure.error);
            }

            Ok(())
        },
    )
    .await?;

    ordered_objects
        .into_iter()
        .enumerate()
        .map(|(index, object)| {
            object.with_context(|| {
                format!(
                    "internal error: missing export result for object index {}",
                    index + 1
                )
            })
        })
        .collect()
}

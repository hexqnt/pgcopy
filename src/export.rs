use anyhow::{Context, Result, bail};
use chrono::Utc;
use std::env;
use std::path::Path;
use tempfile::TempDir;

use crate::bundle_io;
use crate::config::{self, Config, GeneralConfig, ObjectConfig};
use crate::crypto;
use crate::manifest::{Manifest, ManifestObject};
use crate::pg;

mod object;
mod progress;
mod session;
mod worker;
use object::{export_object, validate_target_collisions};
use progress::ExportProgress;
use session::run_with_snapshot_support;
use worker::export_worker;

/// Выполняет экспорт объектов в bundle.
pub async fn run(
    config_path: &Path,
    out_path: &Path,
    cli_concurrency: Option<usize>,
    bundle_password: Option<&str>,
    source_config: tokio_postgres::Config,
    progress_enabled: bool,
) -> Result<()> {
    let config = config::load(config_path)?;
    let concurrency = resolve_export_concurrency(cli_concurrency, &config.general)?;
    let password = crypto::resolve_bundle_password(bundle_password)?;
    let progress = ExportProgress::new(&config, progress_enabled);

    let client = pg::connect(&source_config).await?;

    let source_pg_version_num = pg::server_version_num(&client).await?;
    let source_fingerprint = Some(pg::source_fingerprint(&client).await?);

    let scratch = tempfile::tempdir().context("failed to create temporary directory for export")?;
    std::fs::create_dir_all(scratch.path().join("ddl"))?;
    std::fs::create_dir_all(scratch.path().join("data"))?;

    let export_result = if concurrency == 1 {
        run_with_snapshot_support(&client, config.general.consistent_snapshot, false, |_| {
            export_objects(&client, &config, &scratch, &progress)
        })
        .await
    } else {
        run_with_snapshot_support(
            &client,
            config.general.consistent_snapshot,
            true,
            |snapshot_id| {
                export_objects_parallel(
                    &source_config,
                    &config,
                    &scratch,
                    &progress,
                    concurrency,
                    snapshot_id,
                )
            },
        )
        .await
    };

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

    progress.set_bundle_running(out_path);
    let bundle_scratch_path = scratch.path().to_path_buf();
    let bundle_out_path = out_path.to_path_buf();
    let bundle_password = password;
    let bundle_manifest = manifest;
    let write_result = tokio::task::spawn_blocking(move || {
        bundle_io::write_bundle(
            &bundle_scratch_path,
            &bundle_out_path,
            &bundle_manifest,
            bundle_password.as_deref(),
        )
    })
    .await
    .context("bundle writer task failed")?;
    match write_result {
        Ok(()) => {
            progress.finish_bundle_done(out_path);
            Ok(())
        }
        Err(error) => {
            progress.finish_bundle_error(out_path, error.as_ref());
            Err(error)
        }
    }
}

fn resolve_export_concurrency(
    cli_concurrency: Option<usize>,
    general: &GeneralConfig,
) -> Result<usize> {
    if let Some(concurrency) = cli_concurrency {
        if concurrency == 0 {
            bail!("export concurrency must be >= 1");
        }
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

            let parsed = trimmed.parse::<usize>().map_err(|_| {
                anyhow::anyhow!("invalid {env_name} value '{trimmed}', expected integer >= 1")
            })?;
            if parsed == 0 {
                bail!("invalid {env_name} value '0', expected integer >= 1");
            }

            Ok(parsed)
        }
        Err(env::VarError::NotPresent) => Ok(general.concurrency),
        Err(env::VarError::NotUnicode(_)) => {
            bail!("environment variable {env_name} contains non-Unicode data")
        }
    }
}

async fn export_objects(
    client: &tokio_postgres::Client,
    config: &Config,
    scratch: &TempDir,
    progress: &ExportProgress,
) -> Result<Vec<ManifestObject>> {
    let mut manifest_objects = Vec::with_capacity(config.objects.len());

    for (index, object) in config.objects.iter().enumerate() {
        progress.set_object_running(index, object);
        let manifest_object = export_object(
            client,
            scratch.path(),
            index,
            object,
            config.general.data_format,
        )
        .await
        .with_context(|| {
            format!(
                "export object {}.{} failed",
                object.select.source_schema, object.select.source_name
            )
        });

        match manifest_object {
            Ok(manifest_object) => {
                progress.set_object_done(index, &manifest_object);
                manifest_objects.push(manifest_object);
            }
            Err(error) => {
                progress.set_object_error(index, object, error.as_ref());
                return Err(error);
            }
        }
    }

    validate_target_collisions(&manifest_objects)?;
    Ok(manifest_objects)
}

async fn export_objects_parallel(
    source_config: &tokio_postgres::Config,
    config: &Config,
    scratch: &TempDir,
    progress: &ExportProgress,
    concurrency: usize,
    snapshot_id: Option<String>,
) -> Result<Vec<ManifestObject>> {
    let data_format = config.general.data_format;
    let mut ordered_objects = vec![None; config.objects.len()];
    let workers_count = concurrency.min(config.objects.len());
    let mut buckets = vec![Vec::<(usize, ObjectConfig)>::new(); workers_count];

    for (index, object) in config.objects.iter().cloned().enumerate() {
        progress.set_object_running(index, &object);
        buckets[index % workers_count].push((index, object));
    }

    let mut workers = tokio::task::JoinSet::new();
    for tasks in buckets.into_iter().filter(|tasks| !tasks.is_empty()) {
        let source_config = source_config.clone();
        let scratch_dir = scratch.path().to_path_buf();
        let snapshot_id = snapshot_id.clone();
        // Каждый worker получает свой connection и обрабатывает свой bucket.
        workers.spawn(async move {
            export_worker(
                &source_config,
                &scratch_dir,
                tasks,
                data_format,
                snapshot_id.as_deref(),
            )
            .await
        });
    }

    while let Some(join_result) = workers.join_next().await {
        let outcome = join_result.context("parallel export worker task failed")?;

        for (index, manifest_object) in outcome.completed {
            progress.set_object_done(index, &manifest_object);
            ordered_objects[index] = Some(manifest_object);
        }

        if let Some(failure) = outcome.failure {
            workers.abort_all();
            while workers.join_next().await.is_some() {}
            progress.set_object_error(failure.index, &failure.object, failure.error.as_ref());
            return Err(failure.error);
        }
    }

    let mut manifest_objects = Vec::with_capacity(config.objects.len());
    for (index, manifest_object) in ordered_objects.into_iter().enumerate() {
        let manifest_object = manifest_object.with_context(|| {
            format!(
                "internal error: missing export result for object index {}",
                index + 1
            )
        })?;
        manifest_objects.push(manifest_object);
    }

    validate_target_collisions(&manifest_objects)?;
    Ok(manifest_objects)
}

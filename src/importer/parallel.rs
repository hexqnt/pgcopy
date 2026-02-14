use std::path::Path;

use anyhow::{Context, Result};

use crate::manifest::{Manifest, ManifestObject};
use crate::types::DataFormat;

use super::{ImportMode, copy_stream, load};

/// Импортирует объекты из распакованного bundle параллельно по bucket-ам.
pub async fn import_objects_parallel(
    target_config: &tokio_postgres::Config,
    scratch_dir: &Path,
    manifest: &Manifest,
    mode: ImportMode,
    concurrency: usize,
) -> Result<()> {
    let data_format = manifest.data_format;
    let workers_count = concurrency.min(manifest.objects.len());
    let mut buckets = vec![Vec::<(usize, ManifestObject)>::new(); workers_count];

    for (index, object) in manifest.objects.iter().cloned().enumerate() {
        buckets[index % workers_count].push((index, object));
    }

    let mut workers = tokio::task::JoinSet::new();
    for tasks in buckets.into_iter().filter(|tasks| !tasks.is_empty()) {
        let target_config = target_config.clone();
        let scratch_dir = scratch_dir.to_path_buf();
        // Каждый worker использует отдельное соединение с target БД.
        workers.spawn(async move {
            import_worker(&target_config, &scratch_dir, tasks, mode, data_format).await
        });
    }

    while let Some(join_result) = workers.join_next().await {
        let worker_result = join_result.context("parallel import worker task failed")?;
        if let Err(error) = worker_result {
            workers.abort_all();
            while workers.join_next().await.is_some() {}
            return Err(error);
        }
    }

    Ok(())
}

async fn import_worker(
    target_config: &tokio_postgres::Config,
    scratch_dir: &Path,
    tasks: Vec<(usize, ManifestObject)>,
    mode: ImportMode,
    data_format: DataFormat,
) -> Result<()> {
    let client = crate::pg::connect(target_config).await.with_context(|| {
        "failed to connect target database for parallel import worker".to_owned()
    })?;

    for (index, object) in tasks {
        import_object(&client, scratch_dir, &object, mode, data_format)
            .await
            .with_context(|| {
                format!(
                    "manifest object index {} ({}.{})",
                    index + 1,
                    object.target_schema,
                    object.target_name
                )
            })?;
    }

    Ok(())
}

async fn import_object(
    client: &tokio_postgres::Client,
    scratch_dir: &Path,
    object: &ManifestObject,
    mode: ImportMode,
    data_format: DataFormat,
) -> Result<()> {
    let ddl_sql = read_ddl_from_bundle(scratch_dir, &object.ddl_path).await?;
    let data_path = scratch_dir.join(&object.data_path);
    load::load_object(client, object, mode, &ddl_sql, || async {
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

    Ok(())
}

async fn read_ddl_from_bundle(scratch_dir: &Path, ddl_rel_path: &str) -> Result<String> {
    let ddl_path = scratch_dir.join(ddl_rel_path);
    tokio::fs::read_to_string(&ddl_path)
        .await
        .with_context(|| format!("failed to read DDL file {}", ddl_path.display()))
}

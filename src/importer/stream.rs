use std::io::Read;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};

use crate::bundle_io::BundleAccess;
use crate::manifest::Manifest;

use super::{ImportMode, compat, copy_stream, load, progress::ImportProgress};

pub(super) struct ImportStreamOptions {
    pub(super) access: BundleAccess,
    pub(super) mode: ImportMode,
    pub(super) ddl_only: bool,
    pub(super) target_version_num: i32,
    pub(super) progress_enabled: bool,
}

fn read_ddl_entry<R: Read>(entries: &mut tar::Entries<'_, R>, ddl_path: &str) -> Result<String> {
    let mut ddl_entry = crate::bundle_io::next_required_entry(entries, ddl_path)?;
    let mut ddl_sql = String::new();
    ddl_entry
        .read_to_string(&mut ddl_sql)
        .with_context(|| format!("failed to read DDL entry '{ddl_path}' from bundle"))?;
    Ok(ddl_sql)
}
/// Импортирует bundle в потоковом режиме без предварительной распаковки.
pub async fn import_objects_streaming(
    bundle_path: &Path,
    client: &tokio_postgres::Client,
    options: ImportStreamOptions,
    operation_started_at: Instant,
) -> Result<u64> {
    let reader = crate::bundle_io::open_bundle_reader(bundle_path, &options.access)?;
    let mut archive = tar::Archive::new(reader);
    import_from_archive_stream(
        &mut archive,
        client,
        options.mode,
        options.ddl_only,
        options.target_version_num,
        options.progress_enabled,
        operation_started_at,
    )
    .await
}

async fn import_from_archive_stream<R: Read>(
    archive: &mut tar::Archive<R>,
    client: &tokio_postgres::Client,
    mode: ImportMode,
    ddl_only: bool,
    target_version_num: i32,
    progress_enabled: bool,
    operation_started_at: Instant,
) -> Result<u64> {
    let mut entries = archive
        .entries()
        .context("failed to enumerate bundle archive entries")?;

    let manifest = crate::bundle_io::read_manifest_from_entries(&mut entries)?;
    if !ddl_only {
        compat::validate_data_compatibility(&manifest, target_version_num)?;
    }
    let progress = ImportProgress::new(&manifest, progress_enabled);
    let total_rows =
        import_objects_layout_v2(&mut entries, client, mode, ddl_only, &manifest, &progress)
            .await?;
    progress.finish_done(total_rows, operation_started_at.elapsed());
    Ok(total_rows)
}

async fn import_objects_layout_v2<R: Read>(
    entries: &mut tar::Entries<'_, R>,
    client: &tokio_postgres::Client,
    mode: ImportMode,
    ddl_only: bool,
    manifest: &Manifest,
    progress: &ImportProgress,
) -> Result<u64> {
    // Формат v2: после manifest идут все DDL, затем все data.
    let mut ddl_entries = Vec::with_capacity(manifest.objects.len());
    for object in &manifest.objects {
        ddl_entries.push(read_ddl_entry(entries, &object.ddl_path)?);
    }

    let mut total_rows = 0_u64;
    for ((index, object), ddl_sql) in manifest.objects.iter().enumerate().zip(&ddl_entries) {
        progress.set_object_running(object);
        let object_started_at = Instant::now();
        let import_result: Result<u64> = async {
            let imported_rows = if ddl_only {
                load::prepare_object_ddl_only(client, object, mode, ddl_sql).await?;
                0
            } else if !object.requires_data_load() {
                load::prepare_object_ddl_only(client, object, mode, ddl_sql).await?;
                let mut data_entry =
                    crate::bundle_io::next_required_entry(entries, &object.data_path)?;
                // Для view payload data отсутствует; entry читаем, чтобы сохранить строгий layout.
                let _ =
                    std::io::copy(&mut data_entry, &mut std::io::sink()).with_context(|| {
                        format!(
                            "failed to consume data entry '{}' from bundle",
                            object.data_path
                        )
                    })?;
                0
            } else {
                load::load_object(client, object, mode, ddl_sql, || async {
                    let mut data_entry =
                        crate::bundle_io::next_required_entry(entries, &object.data_path)?;
                    let mut source = copy_stream::ReaderChunkSource::new(&mut data_entry);
                    copy_stream::copy_data_in(
                        client,
                        &mut source,
                        &object.target_schema,
                        &object.target_name,
                        &object.effective_columns,
                        manifest.data_format,
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "failed to import data entry '{}' into {}.{}",
                            object.data_path, object.target_schema, object.target_name
                        )
                    })
                })
                .await?
            };

            Ok::<u64, anyhow::Error>(imported_rows)
        }
        .await
        .with_context(|| {
            format!(
                "manifest object index {} ({}.{})",
                index + 1,
                object.target_schema,
                object.target_name
            )
        });

        match import_result {
            Ok(inserted_rows) => {
                total_rows += inserted_rows;
                progress.set_object_done(object, inserted_rows, object_started_at.elapsed());
            }
            Err(error) => {
                progress.set_object_error(object, error.as_ref());
                progress.finish_with_error(error.as_ref());
                return Err(error);
            }
        }
    }

    Ok(total_rows)
}


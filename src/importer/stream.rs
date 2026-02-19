use std::io::{self, Read};
use std::path::Path;

use anyhow::{Context, Result};

use super::{ImportMode, compat, copy_stream, load, progress::ImportProgress};

/// Импортирует bundle в потоковом режиме без предварительной распаковки.
pub async fn import_objects_streaming(
    bundle_path: &Path,
    password: Option<&str>,
    is_encrypted: bool,
    client: &tokio_postgres::Client,
    mode: ImportMode,
    ddl_only: bool,
    target_version_num: i32,
    progress_enabled: bool,
) -> Result<u64> {
    let reader = crate::bundle_io::open_bundle_reader(bundle_path, password, is_encrypted)?;
    let mut archive = tar::Archive::new(reader);
    import_from_archive_stream(
        &mut archive,
        client,
        mode,
        ddl_only,
        target_version_num,
        progress_enabled,
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
) -> Result<u64> {
    let mut entries = archive
        .entries()
        .context("failed to enumerate bundle archive entries")?;

    let manifest = crate::bundle_io::read_manifest_from_entries(&mut entries)?;
    if !ddl_only {
        compat::validate_data_compatibility(&manifest, target_version_num)?;
    }
    let progress = ImportProgress::new(&manifest, progress_enabled);
    let mut total_rows = 0_u64;

    for (index, object) in manifest.objects.iter().enumerate() {
        progress.set_object_running(index, object);
        let import_result: Result<u64> = async {
            let ddl_sql = {
                // Порядок archive entries строго фиксирован manifest-ом.
                let mut ddl_entry =
                    crate::bundle_io::next_required_entry(&mut entries, &object.ddl_path)?;
                let mut ddl_sql = String::new();
                ddl_entry.read_to_string(&mut ddl_sql).with_context(|| {
                    format!("failed to read DDL entry '{}' from bundle", object.ddl_path)
                })?;
                ddl_sql
            };

            let imported_rows = if ddl_only {
                load::prepare_object_ddl_only(client, object, mode, &ddl_sql).await?;
                skip_data_entry(&mut entries, &object.data_path).await?;
                0
            } else {
                load::load_object(client, object, mode, &ddl_sql, || async {
                    let mut data_entry =
                        crate::bundle_io::next_required_entry(&mut entries, &object.data_path)?;
                    copy_stream::copy_data_in_reader(
                        client,
                        &mut data_entry,
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
                progress.set_object_done(index, object, inserted_rows);
            }
            Err(error) => {
                progress.set_object_error(index, object, error.as_ref());
                progress.finish_with_error(error.as_ref());
                return Err(error);
            }
        }
    }

    progress.finish_done(total_rows);
    Ok(total_rows)
}

async fn skip_data_entry<R: Read>(
    entries: &mut tar::Entries<'_, R>,
    data_path: &str,
) -> Result<()> {
    let mut data_entry = crate::bundle_io::next_required_entry(entries, data_path)?;
    tokio::task::block_in_place(|| io::copy(&mut data_entry, &mut io::sink()))
        .with_context(|| format!("failed to skip data entry '{}' from bundle", data_path))?;
    Ok(())
}

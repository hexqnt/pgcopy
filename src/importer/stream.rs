use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};

use super::{ImportMode, compat, copy_stream, load};

/// Импортирует bundle в потоковом режиме без предварительной распаковки.
pub async fn import_objects_streaming(
    bundle_path: &Path,
    password: Option<&str>,
    is_encrypted: bool,
    client: &tokio_postgres::Client,
    mode: ImportMode,
    target_version_num: i32,
) -> Result<()> {
    let reader = crate::bundle_io::open_bundle_reader(bundle_path, password, is_encrypted)?;
    let mut archive = tar::Archive::new(reader);
    import_from_archive_stream(&mut archive, client, mode, target_version_num).await?;

    Ok(())
}

async fn import_from_archive_stream<R: Read>(
    archive: &mut tar::Archive<R>,
    client: &tokio_postgres::Client,
    mode: ImportMode,
    target_version_num: i32,
) -> Result<()> {
    let mut entries = archive
        .entries()
        .context("failed to enumerate bundle archive entries")?;

    let manifest = crate::bundle_io::read_manifest_from_entries(&mut entries)?;
    compat::validate_data_compatibility(&manifest, target_version_num)?;

    for (index, object) in manifest.objects.iter().enumerate() {
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

use anyhow::{Result, bail};

use crate::manifest::ManifestObject;
use crate::types::ExportAs;

use super::{ImportMode, replace_tx, sequences, target_table};

/// Общий шаг загрузки одного объекта: подготовка target, при необходимости COPY + sync sequence.
///
/// Фактический источник данных (`file` или `reader`) передаётся через `copy_data`.
pub(super) async fn load_object<C, Fut>(
    client: &tokio_postgres::Client,
    object: &ManifestObject,
    mode: ImportMode,
    ddl_sql: &str,
    copy_data: C,
) -> Result<u64>
where
    C: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<u64>>,
{
    ensure_mode_supported_for_object(object, mode)?;

    run_with_mode(client, object, mode, move || async move {
        target_table::prepare_target_table(client, object, mode, ddl_sql).await?;
        if !object.requires_data_load() {
            return Ok(0);
        }

        let inserted_rows = copy_data().await?;
        sequences::sync_table_sequences(
            client,
            &object.target_schema,
            &object.target_name,
            &object.effective_columns,
        )
        .await?;
        Ok(inserted_rows)
    })
    .await
}

/// Выполняет только DDL-подготовку целевого объекта без загрузки данных.
pub(super) async fn prepare_object_ddl_only(
    client: &tokio_postgres::Client,
    object: &ManifestObject,
    mode: ImportMode,
    ddl_sql: &str,
) -> Result<()> {
    ensure_mode_supported_for_object(object, mode)?;

    run_with_mode(client, object, mode, move || async move {
        target_table::prepare_target_table(client, object, mode, ddl_sql).await
    })
    .await
}

fn ensure_mode_supported_for_object(object: &ManifestObject, mode: ImportMode) -> Result<()> {
    if mode == ImportMode::Append && object.export_as == ExportAs::View {
        bail!(
            "append mode is not supported for view object {}.{}",
            object.target_schema,
            object.target_name
        );
    }
    Ok(())
}

async fn run_with_mode<T, F, Fut>(
    client: &tokio_postgres::Client,
    object: &ManifestObject,
    mode: ImportMode,
    operation: F,
) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    match mode {
        ImportMode::Replace => {
            replace_tx::run_replace_atomically(client, object, operation()).await
        }
        ImportMode::Append => operation().await,
    }
}

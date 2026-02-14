use anyhow::Result;

use crate::manifest::ManifestObject;

use super::{ImportMode, replace_tx, sequences, target_table};

/// Общий шаг загрузки одного объекта: подготовка таблицы, COPY, sync sequence.
///
/// Фактический источник данных (`file` или `reader`) передаётся через `copy_data`.
pub(super) async fn load_object<C, Fut>(
    client: &tokio_postgres::Client,
    object: &ManifestObject,
    mode: ImportMode,
    ddl_sql: &str,
    copy_data: C,
) -> Result<()>
where
    C: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    match mode {
        ImportMode::Replace => {
            replace_tx::run_replace_atomically(client, object, async {
                target_table::prepare_target_table(client, object, ImportMode::Replace, ddl_sql)
                    .await?;
                copy_data().await?;
                sequences::sync_table_sequences(
                    client,
                    &object.target_schema,
                    &object.target_name,
                    &object.effective_columns,
                )
                .await?;
                Ok(())
            })
            .await
        }
        ImportMode::Append => {
            target_table::prepare_target_table(client, object, ImportMode::Append, ddl_sql).await?;
            copy_data().await?;
            sequences::sync_table_sequences(
                client,
                &object.target_schema,
                &object.target_name,
                &object.effective_columns,
            )
            .await?;
            Ok(())
        }
    }
}

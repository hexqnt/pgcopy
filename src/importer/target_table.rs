use anyhow::{Context, Result, bail};

use crate::manifest::ManifestObject;
use crate::pg::{self, RelationKind};
use crate::sql::{quote_ident, quoted_fq_name};

use super::ImportMode;
use super::compat;

/// Подготавливает target-объект под текущий режим импорта.
///
/// - `Replace`: удаляет существующий объект (table/view/mview) и исполняет DDL.
/// - `Append`: создаёт таблицу при отсутствии или валидирует совместимость.
pub(super) async fn prepare_target_table(
    client: &tokio_postgres::Client,
    object: &ManifestObject,
    mode: ImportMode,
    ddl_sql: &str,
) -> Result<()> {
    let create_schema_sql = format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_ident(&object.target_schema)
    );
    client
        .batch_execute(&create_schema_sql)
        .await
        .with_context(|| {
            format!(
                "failed to create schema '{}' for target object {}.{}",
                object.target_schema, object.target_schema, object.target_name
            )
        })?;

    let target_kind = pg::relation_kind_opt(client, &object.target_schema, &object.target_name)
        .await
        .with_context(|| {
            format!(
                "failed to inspect target relation {}.{}",
                object.target_schema, object.target_name
            )
        })?;

    match mode {
        ImportMode::Replace => {
            if let Some(kind) = target_kind {
                match kind {
                    RelationKind::Table => {
                        let drop_sql = format!(
                            "DROP TABLE IF EXISTS {}",
                            quoted_fq_name(&object.target_schema, &object.target_name)
                        );
                        client.batch_execute(&drop_sql).await.with_context(|| {
                            format!(
                                "failed to drop target table {}.{}",
                                object.target_schema, object.target_name
                            )
                        })?;
                    }
                    RelationKind::View => {
                        let drop_sql = format!(
                            "DROP VIEW IF EXISTS {}",
                            quoted_fq_name(&object.target_schema, &object.target_name)
                        );
                        client.batch_execute(&drop_sql).await.with_context(|| {
                            format!(
                                "failed to drop target view {}.{}",
                                object.target_schema, object.target_name
                            )
                        })?;
                    }
                    RelationKind::MaterializedView => {
                        let drop_sql = format!(
                            "DROP MATERIALIZED VIEW IF EXISTS {}",
                            quoted_fq_name(&object.target_schema, &object.target_name)
                        );
                        client.batch_execute(&drop_sql).await.with_context(|| {
                            format!(
                                "failed to drop target materialized view {}.{}",
                                object.target_schema, object.target_name
                            )
                        })?;
                    }
                }
            }

            execute_ddl_sql(client, ddl_sql, &object.target_schema, &object.target_name).await?;
        }
        ImportMode::Append => match target_kind {
            None => {
                execute_ddl_sql(client, ddl_sql, &object.target_schema, &object.target_name)
                    .await?;
            }
            Some(RelationKind::Table) => {
                compat::validate_existing_table_compatibility(client, object).await?;
            }
            Some(other_kind) => {
                bail!(
                    "append mode requires target {}.{} to be a table, got {}",
                    object.target_schema,
                    object.target_name,
                    other_kind.as_str()
                );
            }
        },
    }

    Ok(())
}

async fn execute_ddl_sql(
    client: &tokio_postgres::Client,
    ddl_sql: &str,
    target_schema: &str,
    target_name: &str,
) -> Result<()> {
    client.batch_execute(ddl_sql).await.with_context(|| {
        format!("failed to execute DDL for target object {target_schema}.{target_name}")
    })?;
    Ok(())
}

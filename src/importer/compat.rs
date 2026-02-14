use anyhow::{Context, Result, bail};

use crate::manifest::{Manifest, ManifestObject};
use crate::pg;
use crate::types::DataFormat;

/// Проверяет общую совместимость bundle и target перед импортом.
pub(super) fn validate_data_compatibility(
    manifest: &Manifest,
    target_version_num: i32,
) -> Result<()> {
    match manifest.data_format {
        DataFormat::Binary => {
            // PostgreSQL binary COPY не гарантирует совместимость между major-версиями.
            let source_major = manifest.source_pg_version_num / 10_000;
            let target_major = target_version_num / 10_000;

            if source_major != target_major {
                bail!(
                    "binary COPY compatibility check failed: source server_version_num={} (major {}), target server_version_num={} (major {})",
                    manifest.source_pg_version_num,
                    source_major,
                    target_version_num,
                    target_major
                );
            }
        }
        DataFormat::Csv => {}
    }

    Ok(())
}

/// Проверяет, что существующая target-таблица подходит для append режима.
pub(super) async fn validate_existing_table_compatibility(
    client: &tokio_postgres::Client,
    object: &ManifestObject,
) -> Result<()> {
    let target_columns =
        pg::relation_columns_with_types(client, &object.target_schema, &object.target_name)
            .await
            .with_context(|| {
                format!(
                    "failed to inspect target table {}.{} for append mode",
                    object.target_schema, object.target_name
                )
            })?;

    let target_types_by_column = target_columns
        .iter()
        .map(|(name, type_sql)| (name.as_str(), type_sql.as_str()))
        .collect::<std::collections::HashMap<_, _>>();

    for (index, column) in object.effective_columns.iter().enumerate() {
        let actual_type = target_types_by_column.get(column.as_str()).with_context(|| {
            format!(
                "append mode compatibility error: target table {}.{} does not contain required column '{}'",
                object.target_schema, object.target_name, column
            )
        })?;

        if object.effective_column_types.len() == object.effective_columns.len() {
            let expected_type = object
                .effective_column_types
                .get(index)
                .with_context(|| {
                    format!(
                        "append mode compatibility error: expected type metadata is incomplete for {}.{} column '{}'",
                        object.target_schema, object.target_name, column
                    )
                })?;
            if actual_type != &expected_type.as_str() {
                bail!(
                    "append mode compatibility error: column type mismatch for {}.{}.'{}': expected '{}', got '{}'",
                    object.target_schema,
                    object.target_name,
                    column,
                    expected_type,
                    actual_type
                );
            }
        }
    }

    Ok(())
}

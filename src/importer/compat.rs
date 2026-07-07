use anyhow::{Context, Result, bail};

use crate::manifest::{Manifest, ManifestObject};
use crate::pg;
use crate::types::DataFormat;

/// Проверяет общую совместимость bundle и target перед импортом.
pub(super) fn validate_data_compatibility(
    manifest: &Manifest,
    target_version_num: i32,
) -> Result<()> {
    if !manifest
        .objects
        .iter()
        .any(ManifestObject::requires_data_load)
    {
        return Ok(());
    }

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

    let actual_type_for = |column: &str| {
        target_types_by_column.get(column).with_context(|| {
            format!(
                "append mode compatibility error: target table {}.{} does not contain required column '{}'",
                object.target_schema, object.target_name, column
            )
        })
    };

    if object.effective_column_types.len() == object.effective_columns.len() {
        for (column, expected_type) in object
            .effective_columns
            .iter()
            .zip(&object.effective_column_types)
        {
            let actual_type = actual_type_for(column.as_str())?;
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
    } else {
        for column in &object.effective_columns {
            actual_type_for(column.as_str())?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_data_compatibility;
    use crate::manifest::{Manifest, ManifestObject};
    use crate::pg::RelationKind;
    use crate::select_dsl::ProjectionKind;
    use crate::types::{DataFormat, ExportAs};

    fn manifest_for_compat(data_format: DataFormat, source_pg_version_num: i32) -> Manifest {
        Manifest {
            format_version: 2,
            created_at: "2026-02-19T10:00:00Z".to_owned(),
            source_fingerprint: Some("database=app user=app".to_owned()),
            source_pg_version_num,
            data_format,
            consistent_snapshot: true,
            objects: vec![ManifestObject {
                kind: RelationKind::Table,
                export_as: ExportAs::Table,
                source_schema: "public".to_owned(),
                source_name: "orders".to_owned(),
                target_schema: "archive".to_owned(),
                target_name: "orders".to_owned(),
                source_select: "select * from public.orders".to_owned(),
                normalized_select: "SELECT \"id\" FROM \"public\".\"orders\"".to_owned(),
                ddl_path: "ddl/0001__public.orders.sql".to_owned(),
                data_path: "data/0001__public.orders.copybin".to_owned(),
                effective_columns: vec!["id".to_owned()],
                effective_column_types: vec!["bigint".to_owned()],
                column_projection: ProjectionKind::All,
                row_estimate: Some(10),
            }],
        }
    }

    #[test]
    fn binary_compatibility_passes_for_same_major() {
        let manifest = manifest_for_compat(DataFormat::Binary, 150_002);
        validate_data_compatibility(&manifest, 150_099)
            .expect("binary format should be compatible on same major version");
    }

    #[test]
    fn binary_compatibility_fails_for_different_major() {
        let manifest = manifest_for_compat(DataFormat::Binary, 140_012);
        let error = validate_data_compatibility(&manifest, 150_001)
            .expect_err("binary format must fail across major versions");
        assert!(
            error
                .to_string()
                .contains("binary COPY compatibility check failed")
        );
    }

    #[test]
    fn csv_compatibility_ignores_major_version_difference() {
        let manifest = manifest_for_compat(DataFormat::Csv, 140_012);
        validate_data_compatibility(&manifest, 160_003)
            .expect("csv format should ignore major version difference");
    }
}

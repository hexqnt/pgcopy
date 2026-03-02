use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::types::DataFormat;

/// Метаданные bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Версия схемы manifest (поддерживается только `2`).
    pub format_version: u32,
    /// RFC3339 timestamp создания bundle.
    pub created_at: String,
    /// Краткий fingerprint источника (database/user).
    pub source_fingerprint: Option<String>,
    /// Значение `SHOW server_version_num` на source.
    pub source_pg_version_num: i32,
    /// Формат данных в файлах `data/*`.
    pub data_format: DataFormat,
    /// Признак, что экспорт делался в consistent snapshot.
    pub consistent_snapshot: bool,
    /// Описания объектов в bundle.
    pub objects: Vec<ManifestObject>,
}

/// Метаданные одного выгруженного объекта.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestObject {
    pub kind: String,
    pub source_schema: String,
    pub source_name: String,
    pub target_schema: String,
    pub target_name: String,
    pub source_select: String,
    pub normalized_select: String,
    pub ddl_path: String,
    pub data_path: String,
    pub effective_columns: Vec<String>,
    #[serde(default)]
    pub effective_column_types: Vec<String>,
    pub column_projection: String,
    pub row_estimate: Option<i64>,
}

/// Парсит и валидирует manifest, защищая импорт от неконсистентных bundle.
pub fn parse_manifest(manifest_raw: &str, manifest_source: &str) -> Result<Manifest> {
    use std::collections::HashSet;

    let manifest = serde_json::from_str::<Manifest>(manifest_raw)
        .with_context(|| format!("failed to parse manifest JSON {manifest_source}"))?;

    if manifest.objects.is_empty() {
        bail!("manifest validation error: objects must not be empty");
    }

    if manifest.format_version != 2 {
        bail!(
            "manifest validation error: unsupported format_version {}, expected 2",
            manifest.format_version
        );
    }

    let mut seen_targets = HashSet::new();
    for (index, object) in manifest.objects.iter().enumerate() {
        if object.effective_columns.is_empty() {
            bail!(
                "manifest validation error: objects[{index}] has empty effective_columns for {}.{}",
                object.target_schema,
                object.target_name
            );
        }

        if !object.effective_column_types.is_empty()
            && object.effective_column_types.len() != object.effective_columns.len()
        {
            bail!(
                "manifest validation error: objects[{index}] has mismatched effective_column_types ({}) and effective_columns ({}) lengths",
                object.effective_column_types.len(),
                object.effective_columns.len()
            );
        }

        let target_key = format!("{}.{}", object.target_schema, object.target_name);
        if !seen_targets.insert(target_key.clone()) {
            bail!(
                "manifest validation error: duplicate target object {} at objects[{index}]",
                target_key
            );
        }
    }

    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::parse_manifest;

    fn base_object(target_schema: &str, target_name: &str) -> Value {
        json!({
            "kind": "table",
            "source_schema": "public",
            "source_name": "orders",
            "target_schema": target_schema,
            "target_name": target_name,
            "source_select": "select * from public.orders",
            "normalized_select": "SELECT \"id\" FROM \"public\".\"orders\"",
            "ddl_path": "ddl/0001__public.orders.sql",
            "data_path": "data/0001__public.orders.copybin",
            "effective_columns": ["id"],
            "effective_column_types": ["bigint"],
            "column_projection": "*",
            "row_estimate": 10
        })
    }

    fn manifest_raw_with_objects(objects: Vec<Value>) -> String {
        json!({
            "format_version": 2,
            "created_at": "2026-02-19T10:00:00Z",
            "source_fingerprint": "database=app user=app",
            "source_pg_version_num": 150002,
            "data_format": "binary",
            "consistent_snapshot": true,
            "objects": objects
        })
        .to_string()
    }

    #[test]
    fn parses_valid_manifest() {
        let raw = manifest_raw_with_objects(vec![base_object("archive", "orders")]);
        let manifest = parse_manifest(&raw, "inline manifest").expect("manifest should parse");
        assert_eq!(manifest.format_version, 2);
        assert_eq!(manifest.objects.len(), 1);
        assert_eq!(manifest.objects[0].target_schema, "archive");
        assert_eq!(manifest.objects[0].target_name, "orders");
    }

    #[test]
    fn rejects_duplicate_target_objects() {
        let raw = manifest_raw_with_objects(vec![
            base_object("archive", "orders"),
            base_object("archive", "orders"),
        ]);
        let error = parse_manifest(&raw, "inline manifest")
            .expect_err("duplicate target objects must fail validation");
        assert!(
            error
                .to_string()
                .contains("duplicate target object archive.orders")
        );
    }

    #[test]
    fn rejects_mismatched_column_types_length() {
        let mut object = base_object("archive", "orders");
        object["effective_columns"] = json!(["id", "status"]);
        object["effective_column_types"] = json!(["bigint"]);
        let raw = manifest_raw_with_objects(vec![object]);

        let error = parse_manifest(&raw, "inline manifest")
            .expect_err("mismatched effective_column_types must fail validation");
        assert!(
            error
                .to_string()
                .contains("mismatched effective_column_types")
        );
    }

    #[test]
    fn rejects_unsupported_format_version() {
        let mut root: Value = serde_json::from_str(&manifest_raw_with_objects(vec![base_object(
            "archive", "orders",
        )]))
        .expect("test json must parse");
        root["format_version"] = json!(1);

        let error = parse_manifest(&root.to_string(), "inline manifest")
            .expect_err("unsupported format version must fail validation");
        assert!(error.to_string().contains("unsupported format_version 1"));
    }
}

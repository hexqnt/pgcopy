use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::types::DataFormat;

/// Метаданные bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Версия схемы manifest (сейчас поддерживается только `1`).
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

    if manifest.format_version != 1 {
        bail!(
            "manifest validation error: unsupported format_version {}, expected 1",
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

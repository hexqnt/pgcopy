use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::select_dsl::SelectDsl;
use crate::sql::ensure_identifier;
use crate::types::DataFormat;

/// Нормализованный конфиг экспорта после валидации TOML.
#[derive(Debug, Clone)]
pub struct Config {
    /// Глобальные параметры экспорта.
    pub general: GeneralConfig,
    /// Список объектов для экспорта в порядке из конфигурации.
    pub objects: Vec<ObjectConfig>,
}

/// Общие настройки экспорта.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct GeneralConfig {
    /// Формат COPY-потока в bundle.
    pub data_format: DataFormat,
    /// Алгоритм сжатия bundle (сейчас поддерживается только `zstd`).
    pub compression: String,
    /// Включает REPEATABLE READ snapshot для согласованного чтения.
    pub consistent_snapshot: bool,
    /// Количество параллельных workers.
    pub concurrency: usize,
    /// `true`, если значение concurrency пришло из TOML, а не из дефолта.
    pub concurrency_from_toml: bool,
}

/// Описание одного объекта экспорта.
#[derive(Debug, Clone)]
pub struct ObjectConfig {
    /// Исходная строка DSL (полезно для ошибок и дебага).
    pub select_raw: String,
    /// Распарсенная и нормализованная форма `select_raw`.
    pub select: SelectDsl,
    /// Явно заданная целевая схема (опционально).
    pub target_schema: Option<String>,
    /// Явно заданное целевое имя объекта (опционально).
    pub target_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    general: RawGeneral,
    #[serde(default)]
    objects: Vec<RawObject>,
}

#[derive(Debug, Deserialize)]
struct RawGeneral {
    #[serde(default = "default_data_format")]
    data_format: DataFormat,
    #[serde(default = "default_compression")]
    compression: String,
    #[serde(default = "default_true")]
    consistent_snapshot: bool,
    concurrency: Option<usize>,
}

impl Default for RawGeneral {
    fn default() -> Self {
        Self {
            data_format: default_data_format(),
            compression: default_compression(),
            consistent_snapshot: default_true(),
            concurrency: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawObject {
    select: String,
    target_schema: Option<String>,
    target_name: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_data_format() -> DataFormat {
    DataFormat::Binary
}

fn default_compression() -> String {
    "zstd".to_owned()
}

fn default_concurrency() -> usize {
    1
}

pub fn load(path: &Path) -> Result<Config> {
    // Файл читаем целиком: это позволяет выдавать валидационные ошибки
    // со ссылкой на исходный путь и не усложняет формат парсером-потоком.
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;

    let parsed: RawConfig = toml::from_str(&raw)
        .with_context(|| format!("failed to parse TOML config {}", path.display()))?;

    if parsed.objects.is_empty() {
        bail!("config validation error: objects must not be empty");
    }

    if !parsed.general.compression.eq_ignore_ascii_case("zstd") {
        bail!(
            "config validation error: only compression = 'zstd' is supported now, got '{}'",
            parsed.general.compression
        );
    }

    if parsed.general.concurrency == Some(0) {
        bail!("config validation error: general.concurrency must be >= 1");
    }

    let mut objects = Vec::with_capacity(parsed.objects.len());
    for (index, object) in parsed.objects.into_iter().enumerate() {
        if object.select.trim().is_empty() {
            bail!("config validation error: objects[{index}].select must not be empty");
        }

        if object.target_schema.is_some() ^ object.target_name.is_some() {
            bail!(
                "config validation error: objects[{index}] must set both target_schema and target_name or neither"
            );
        }

        if let Some(target_schema) = object.target_schema.as_deref() {
            ensure_identifier(target_schema, "target_schema")
                .with_context(|| format!("config validation error in objects[{index}]"))?;
        }

        if let Some(target_name) = object.target_name.as_deref() {
            ensure_identifier(target_name, "target_name")
                .with_context(|| format!("config validation error in objects[{index}]"))?;
        }

        let select = SelectDsl::parse(&object.select)
            .with_context(|| format!("config validation error in objects[{index}].select"))?;

        objects.push(ObjectConfig {
            select_raw: object.select.trim().to_owned(),
            select,
            target_schema: object.target_schema,
            target_name: object.target_name,
        });
    }

    let concurrency_from_toml = parsed.general.concurrency.is_some();
    let concurrency = parsed
        .general
        .concurrency
        .unwrap_or_else(default_concurrency);

    Ok(Config {
        general: GeneralConfig {
            data_format: parsed.general.data_format,
            compression: parsed.general.compression,
            consistent_snapshot: parsed.general.consistent_snapshot,
            concurrency,
            concurrency_from_toml,
        },
        objects,
    })
}

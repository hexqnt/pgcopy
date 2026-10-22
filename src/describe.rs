mod catalog;
mod external;
mod render;

use std::collections::HashSet;
use std::fmt;
use std::io::Write as _;
use std::num::NonZeroUsize;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::Serialize;

use crate::config::Config;
use crate::pg::{self, RelationKind};

/// Формат контекста, создаваемого командой `describe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum OutputFormat {
    #[value(alias = "md", help = "AI-friendly Markdown with embedded SQL")]
    Markdown,
    #[value(help = "DDL and SQL comments only")]
    Sql,
    #[value(help = "Stable machine-readable JSON")]
    Json,
}

impl OutputFormat {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Sql => "sql",
            Self::Json => "json",
        }
    }
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Уровень безопасного профилирования данных.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ProfileMode {
    None,
    Statistics,
}

/// Полный режим сбора статистики после разбора CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatisticsMode {
    Disabled,
    Basic,
    WithCommonValues(NonZeroUsize),
}

impl StatisticsMode {
    const fn profile(self) -> ProfileMode {
        match self {
            Self::Disabled => ProfileMode::None,
            Self::Basic | Self::WithCommonValues(_) => ProfileMode::Statistics,
        }
    }

    pub(crate) const fn is_enabled(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    pub(crate) const fn top_values(self) -> Option<NonZeroUsize> {
        match self {
            Self::Disabled | Self::Basic => None,
            Self::WithCommonValues(limit) => Some(limit),
        }
    }
}

impl From<Option<StatisticsSelection>> for StatisticsMode {
    fn from(selection: Option<StatisticsSelection>) -> Self {
        match selection {
            None => Self::Disabled,
            Some(StatisticsSelection::Basic) => Self::Basic,
            Some(StatisticsSelection::WithCommonValues(limit)) => Self::WithCommonValues(limit),
        }
    }
}

impl fmt::Display for StatisticsMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("disabled"),
            Self::Basic => f.write_str("basic"),
            Self::WithCommonValues(limit) => write!(f, "top {limit} values"),
        }
    }
}

/// Настройка статистики, разобранная непосредственно из CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatisticsSelection {
    Basic,
    WithCommonValues(NonZeroUsize),
}

impl FromStr for StatisticsSelection {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "basic" {
            return Ok(Self::Basic);
        }
        value
            .parse::<NonZeroUsize>()
            .map(Self::WithCommonValues)
            .map_err(|_| "statistics value must be a positive integer".to_owned())
    }
}

/// Режим дополнительного форматирования уже отрендеренного контекста.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormattingMode {
    Auto,
    Disabled,
}

impl FormattingMode {
    pub(crate) const fn from_no_format(no_format: bool) -> Self {
        if no_format {
            Self::Disabled
        } else {
            Self::Auto
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Disabled => "disabled",
        }
    }

    const fn is_enabled(self) -> bool {
        matches!(self, Self::Auto)
    }
}

impl fmt::Display for FormattingMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum IdentityGeneration {
    Always,
    ByDefault,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum GeneratedStorage {
    Stored,
    Virtual,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ConstraintKind {
    PrimaryKey,
    Unique,
    ForeignKey,
    Check,
    Exclusion,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum StatisticsStatus {
    Available,
    Unavailable,
}

#[derive(Debug, Serialize)]
pub(crate) struct DatabaseDescription {
    format_version: u32,
    postgres_version: String,
    postgres_version_num: i32,
    profile: ProfileMode,
    relations: Vec<RelationDescription>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct RelationDescription {
    schema: String,
    name: String,
    kind: RelationKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    comment: Option<String>,
    columns: Vec<ColumnDescription>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    constraints: Vec<ConstraintDescription>,
    #[serde(skip_serializing_if = "Option::is_none")]
    view_definition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    row_estimate: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    statistics_status: Option<StatisticsStatus>,
}

#[derive(Debug, Serialize)]
struct ColumnDescription {
    name: String,
    type_sql: String,
    nullable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_expression: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity: Option<IdentityGeneration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generated: Option<GeneratedColumn>,
    #[serde(skip_serializing_if = "Option::is_none")]
    comment: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    enum_values: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    statistics: Option<ColumnStatistics>,
    #[serde(skip)]
    binary: bool,
}

#[derive(Debug, Serialize)]
struct GeneratedColumn {
    expression: String,
    storage: GeneratedStorage,
}

#[derive(Debug, Serialize)]
struct ConstraintDescription {
    name: String,
    kind: ConstraintKind,
    definition: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    referenced_relation: Option<RelationName>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
struct RelationName {
    schema: String,
    name: String,
}

#[derive(Debug, Serialize)]
struct ColumnStatistics {
    null_fraction: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    estimated_distinct: Option<f64>,
    average_width_bytes: i32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    most_common_values: Vec<CommonValue>,
}

#[derive(Debug, Serialize)]
struct CommonValue {
    value: serde_json::Value,
    frequency: f64,
}

#[derive(Debug, Clone, Copy)]
struct SourceRelation<'a> {
    schema: &'a str,
    name: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Options {
    pub output_format: OutputFormat,
    pub statistics: StatisticsMode,
    pub include_comments: bool,
    pub formatting: FormattingMode,
}

fn source_relations(config: &Config) -> impl Iterator<Item = SourceRelation<'_>> {
    let mut seen = HashSet::with_capacity(config.objects.len());
    config.objects.iter().filter_map(move |object| {
        let source = SourceRelation {
            schema: object.source_schema(),
            name: object.source_name(),
        };
        seen.insert((source.schema, source.name)).then_some(source)
    })
}

pub(crate) fn unique_source_count(config: &Config) -> usize {
    source_relations(config).count()
}

fn output_directory(path: Option<&Path>) -> &Path {
    path.and_then(Path::parent)
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let output_dir = output_directory(Some(path));
    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("failed to create output directory {}", output_dir.display()))?;

    let mut temporary = tempfile::Builder::new()
        .prefix(".pgcopy-describe-")
        .tempfile_in(output_dir)
        .with_context(|| {
            format!(
                "failed to create temporary description file in {}",
                output_dir.display()
            )
        })?;
    temporary
        .write_all(contents)
        .with_context(|| format!("failed to write schema context for {}", path.display()))?;
    temporary.as_file().sync_all().with_context(|| {
        format!(
            "failed to synchronize schema context for {}",
            path.display()
        )
    })?;
    temporary
        .persist(path)
        .with_context(|| format!("failed to publish schema context file {}", path.display()))?;
    Ok(())
}

/// Собирает описание source-объектов и выводит его без чтения строк таблиц.
pub(crate) async fn run(
    config: &Config,
    out_path: Option<&Path>,
    options: Options,
    source_config: &tokio_postgres::Config,
) -> Result<()> {
    let client = pg::connect(source_config).await?;
    let sources = source_relations(config).collect::<Vec<_>>();
    let catalog_options = catalog::CatalogOptions {
        statistics: options.statistics,
        include_comments: options.include_comments,
    };
    let description = catalog::load(&client, &sources, catalog_options).await?;
    let output = render::render(&description, options.output_format)?;
    let output = if options.formatting.is_enabled() {
        let output_format = options.output_format;
        let formatter_dir = output_directory(out_path).to_owned();
        tokio::task::spawn_blocking(move || external::format(output, output_format, &formatter_dir))
            .await
            .context("external formatter task failed")??
    } else {
        output
    };

    if let Some(path) = out_path {
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || write_atomic(&path, output.as_bytes()))
            .await
            .context("schema context writer task failed")?
    } else {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        lock.write_all(output.as_bytes())
            .context("failed to write schema context to stdout")?;
        lock.flush()
            .context("failed to flush schema context to stdout")
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::source_relations;
    use crate::config;

    #[test]
    fn source_scope_ignores_projection_target_and_duplicates() {
        let mut file = tempfile::NamedTempFile::new().expect("temp file must be created");
        write!(
            file,
            r#"
            [[objects]]
            select = "select id from public.orders where id > 10"
            target_schema = "archive"
            target_name = "paid_orders"

            [[objects]]
            select = "select * from public.orders limit 1"

            [[objects]]
            select = "select * from reporting.sales"
            "#
        )
        .expect("config must be written");
        let config = config::load(file.path()).expect("config must parse");

        let sources = source_relations(&config).collect::<Vec<_>>();
        assert_eq!(sources.len(), 2);
        assert_eq!((sources[0].schema, sources[0].name), ("public", "orders"));
        assert_eq!((sources[1].schema, sources[1].name), ("reporting", "sales"));
    }
}

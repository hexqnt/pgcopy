use std::{num::NonZeroU16, num::NonZeroUsize, path::Path};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::env_dsn::ConnectionOverrides;
use crate::select_dsl::{ProjectionKind, SelectDsl};
use crate::sql::{Identifier, quote_ident};
use crate::types::{DataFormat, ExportAs};

/// Значение порта в TOML: может быть целым числом или строкой с `{VAR_NAME}`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PortValue {
    Integer(u16),
    String(String),
}

/// Нормализованный конфиг экспорта после валидации TOML.
#[derive(Debug, Clone)]
pub struct Config {
    /// Глобальные параметры экспорта.
    pub general: GeneralConfig,
    /// Список объектов для экспорта в порядке из конфигурации.
    pub objects: Vec<ObjectConfig>,
    /// Параметры подключения из секции `[connection]` (если задана).
    pub connection: Option<ConnectionOverrides>,
}

/// Общие настройки экспорта.
#[derive(Debug, Clone)]
pub struct GeneralConfig {
    /// Формат COPY-потока в bundle.
    pub data_format: DataFormat,
    /// Включает REPEATABLE READ snapshot для согласованного чтения.
    pub consistent_snapshot: bool,
    /// Количество параллельных workers.
    pub concurrency: NonZeroUsize,
    /// `true`, если значение concurrency пришло из TOML, а не из дефолта.
    pub concurrency_from_toml: bool,
}

/// Явное переименование target-объекта.
#[derive(Debug, Clone)]
pub struct TargetOverride {
    pub schema: Identifier,
    pub name: Identifier,
}

/// Описание одного объекта экспорта.
#[derive(Debug, Clone)]
pub struct ObjectConfig {
    /// Исходная строка DSL (полезно для ошибок и дебага).
    pub select_raw: String,
    /// Распарсенная и нормализованная форма `select_raw`.
    pub select: SelectDsl,
    /// Явное переименование target-объекта.
    pub target: Option<TargetOverride>,
    /// Режим экспорта: в таблицу (default) или как view.
    pub export_as: ExportAs,
}

impl ObjectConfig {
    pub fn source_schema(&self) -> &str {
        &self.select.source_schema
    }

    pub fn source_name(&self) -> &str {
        &self.select.source_name
    }

    pub fn source_label(&self) -> String {
        format!("{}.{}", self.source_schema(), self.source_name())
    }

    pub fn from_dependency(schema: &str, name: &str) -> Result<Self> {
        let select_raw = format!(
            "select * from {}.{}",
            quote_ident(schema),
            quote_ident(name),
        );
        let select = SelectDsl::parse(&select_raw).with_context(|| {
            format!("failed to build dependency select for source relation {schema}.{name}")
        })?;
        Ok(Self {
            select_raw,
            select,
            target: None,
            export_as: ExportAs::Table,
        })
    }
}

#[derive(Debug, Deserialize)]
struct RawConnection {
    host: Option<String>,
    #[serde(default)]
    port: Option<PortValue>,
    dbname: Option<String>,
    user: Option<String>,
    password: Option<String>,
}

impl RawConnection {
    /// Разрешает `{VAR_NAME}` в значениях и возвращает нормализованные переопределения.
    fn resolve(&self) -> Result<Option<ConnectionOverrides>> {
        let host = self
            .host
            .as_deref()
            .map(interpolate_env)
            .transpose()?
            .flatten();

        let port = match &self.port {
            Some(PortValue::Integer(p)) => Some(
                NonZeroU16::new(*p)
                    .ok_or_else(|| anyhow::anyhow!("connection.port must be non-zero"))?,
            ),
            Some(PortValue::String(s)) => interpolate_env(s)?
                .map(|s| {
                    let p: u16 = s
                        .parse()
                        .with_context(|| format!("connection.port: '{s}' is not a valid port"))?;
                    NonZeroU16::new(p)
                        .ok_or_else(|| anyhow::anyhow!("connection.port must be non-zero"))
                })
                .transpose()?,
            None => None,
        };

        let dbname = self
            .dbname
            .as_deref()
            .map(interpolate_env)
            .transpose()?
            .flatten();

        let user = self
            .user
            .as_deref()
            .map(interpolate_env)
            .transpose()?
            .flatten();

        let password = self
            .password
            .as_deref()
            .map(interpolate_env)
            .transpose()?
            .flatten();

        if host.is_none()
            && port.is_none()
            && dbname.is_none()
            && user.is_none()
            && password.is_none()
        {
            return Ok(None);
        }

        Ok(Some(ConnectionOverrides {
            host,
            port,
            dbname,
            user,
            password,
        }))
    }
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    general: RawGeneral,
    #[serde(default)]
    objects: Vec<RawObject>,
    #[serde(default)]
    connection: Option<RawConnection>,
}

#[derive(Debug, Deserialize)]
struct RawGeneral {
    #[serde(default = "default_data_format")]
    data_format: DataFormat,
    #[serde(default = "default_compression")]
    compression: String,
    #[serde(default = "default_true")]
    consistent_snapshot: bool,
    concurrency: Option<NonZeroUsize>,
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
    #[serde(default)]
    export_as: ExportAs,
}

const fn default_true() -> bool {
    true
}

/// Интерполирует `{VAR_NAME}` в строке значениями переменных окружения.
///
/// Если переменная не установлена — подставляется пустая строка.
/// Если после интерполяции результат пуст — возвращается `None`.
fn interpolate_env(value: &str) -> Result<Option<String>> {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars();

    while let Some(ch) = chars.next() {
        if ch == '{' {
            let mut var_name = String::new();
            loop {
                match chars.next() {
                    Some('}') => break,
                    Some(c) => var_name.push(c),
                    None => {
                        bail!("unclosed '{{' in connection config value '{value}'");
                    }
                }
            }
            let var_value = std::env::var(&var_name).unwrap_or_default();
            result.push_str(&var_value);
        } else {
            result.push(ch);
        }
    }

    let trimmed = result.trim().to_owned();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed))
    }
}

const fn default_data_format() -> DataFormat {
    DataFormat::Binary
}

fn default_compression() -> String {
    "zstd".to_owned()
}

const fn default_concurrency() -> NonZeroUsize {
    NonZeroUsize::MIN
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

    let mut objects = Vec::with_capacity(parsed.objects.len());
    for (index, object) in parsed.objects.into_iter().enumerate() {
        let select_raw = object.select.trim();
        if select_raw.is_empty() {
            bail!("config validation error: objects[{index}].select must not be empty");
        }

        let target = parse_target_override(index, object.target_schema, object.target_name)?;

        let select = SelectDsl::parse(select_raw)
            .with_context(|| format!("config validation error in objects[{index}].select"))?;
        validate_view_export_select(index, object.export_as, &select)?;

        objects.push(ObjectConfig {
            select_raw: select_raw.to_owned(),
            select,
            target,
            export_as: object.export_as,
        });
    }

    let concurrency_from_toml = parsed.general.concurrency.is_some();
    let concurrency = parsed
        .general
        .concurrency
        .unwrap_or_else(default_concurrency);

    let connection = parsed
        .connection
        .as_ref()
        .map(RawConnection::resolve)
        .transpose()?
        .flatten();

    Ok(Config {
        general: GeneralConfig {
            data_format: parsed.general.data_format,
            consistent_snapshot: parsed.general.consistent_snapshot,
            concurrency,
            concurrency_from_toml,
        },
        objects,
        connection,
    })
}

fn parse_target_override(
    index: usize,
    target_schema: Option<String>,
    target_name: Option<String>,
) -> Result<Option<TargetOverride>> {
    match (target_schema, target_name) {
        (None, None) => Ok(None),
        (Some(schema), Some(name)) => {
            let schema = Identifier::parse(&schema, "target_schema")
                .with_context(|| format!("config validation error in objects[{index}]"))?;
            let name = Identifier::parse(&name, "target_name")
                .with_context(|| format!("config validation error in objects[{index}]"))?;
            Ok(Some(TargetOverride { schema, name }))
        }
        _ => bail!(
            "config validation error: objects[{index}] must set both target_schema and target_name or neither"
        ),
    }
}

fn validate_view_export_select(
    index: usize,
    export_as: ExportAs,
    select: &SelectDsl,
) -> Result<()> {
    if export_as != ExportAs::View {
        return Ok(());
    }

    if select.projection_kind() != ProjectionKind::All {
        bail!(
            "config validation error: objects[{index}] export_as='view' requires 'select * from schema.object'"
        );
    }
    if select.where_clause.is_some()
        || select.order_by_clause.is_some()
        || select.limit_clause.is_some()
    {
        bail!(
            "config validation error: objects[{index}] export_as='view' does not allow WHERE/ORDER BY/LIMIT"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::num::NonZeroU16;

    use super::{ExportAs, load};

    fn write_temp_config(raw: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("temp file must be created");
        file.write_all(raw.as_bytes())
            .expect("config file must be written");
        file.flush().expect("config file must be flushed");
        file
    }

    #[test]
    fn defaults_export_as_to_table() {
        let file = write_temp_config(
            r#"
            [general]
            compression = "zstd"

            [[objects]]
            select = "select * from public.orders"
            "#,
        );

        let config = load(file.path()).expect("config should parse");
        assert_eq!(config.objects.len(), 1);
        assert_eq!(config.objects[0].export_as, ExportAs::Table);
    }

    #[test]
    fn rejects_view_export_with_where_clause() {
        let file = write_temp_config(
            r#"
            [general]
            compression = "zstd"

            [[objects]]
            select = "select * from reporting.v_sales where id > 10"
            export_as = "view"
            "#,
        );

        let error = load(file.path()).expect_err("invalid view config must fail");
        assert!(
            error
                .to_string()
                .contains("export_as='view' does not allow WHERE/ORDER BY/LIMIT")
        );
    }

    #[test]
    fn connection_section_absent_is_none() {
        let file = write_temp_config(
            r#"
            [general]
            compression = "zstd"

            [[objects]]
            select = "select * from public.orders"
            "#,
        );
        let config = load(file.path()).expect("config should parse");
        assert!(config.connection.is_none());
    }

    #[test]
    fn connection_section_empty_is_none() {
        let file = write_temp_config(
            r#"
            [general]
            compression = "zstd"

            [connection]

            [[objects]]
            select = "select * from public.orders"
            "#,
        );
        let config = load(file.path()).expect("config should parse");
        assert!(config.connection.is_none());
    }

    #[test]
    fn connection_explicit_values() {
        let file = write_temp_config(
            r#"
            [general]
            compression = "zstd"

            [connection]
            host = "db.example.com"
            port = 5433
            dbname = "mydb"
            user = "myuser"
            password = "secret"

            [[objects]]
            select = "select * from public.orders"
            "#,
        );
        let config = load(file.path()).expect("config should parse");
        let conn = config
            .connection
            .expect("connection section must be present");
        assert_eq!(conn.host.as_deref(), Some("db.example.com"));
        assert_eq!(conn.port.map(NonZeroU16::get), Some(5433));
        assert_eq!(conn.dbname.as_deref(), Some("mydb"));
        assert_eq!(conn.user.as_deref(), Some("myuser"));
        assert_eq!(conn.password.as_deref(), Some("secret"));
    }

    #[test]
    fn connection_partial_fields() {
        let file = write_temp_config(
            r#"
            [general]
            compression = "zstd"

            [connection]
            host = "db.example.com"
            dbname = "mydb"

            [[objects]]
            select = "select * from public.orders"
            "#,
        );
        let config = load(file.path()).expect("config should parse");
        let conn = config
            .connection
            .expect("connection section must be present");
        assert_eq!(conn.host.as_deref(), Some("db.example.com"));
        assert_eq!(conn.dbname.as_deref(), Some("mydb"));
        assert!(conn.port.is_none());
        assert!(conn.user.is_none());
        assert!(conn.password.is_none());
    }

    #[test]
    fn connection_port_as_string_with_env_var() {
        unsafe { std::env::set_var("PGCOPY_TEST_PORT", "9876") };
        let file = write_temp_config(
            r#"
            [general]
            compression = "zstd"

            [connection]
            port = "{PGCOPY_TEST_PORT}"

            [[objects]]
            select = "select * from public.orders"
            "#,
        );
        let config = load(file.path()).expect("config should parse");
        let conn = config
            .connection
            .expect("connection section must be present");
        assert_eq!(conn.port.map(NonZeroU16::get), Some(9876));
        unsafe { std::env::remove_var("PGCOPY_TEST_PORT") };
    }

    #[test]
    fn connection_port_zero_rejected() {
        let file = write_temp_config(
            r#"
            [general]
            compression = "zstd"

            [connection]
            port = 0

            [[objects]]
            select = "select * from public.orders"
            "#,
        );
        let error = load(file.path()).expect_err("zero port must fail");
        assert!(
            error
                .to_string()
                .contains("connection.port must be non-zero")
        );
    }

    #[test]
    fn connection_env_var_in_value() {
        unsafe { std::env::set_var("PGCOPY_TEST_HOST", "myhost.example.com") };
        let file = write_temp_config(
            r#"
            [general]
            compression = "zstd"

            [connection]
            host = "{PGCOPY_TEST_HOST}"
            dbname = "app_{PGCOPY_TEST_HOST}"

            [[objects]]
            select = "select * from public.orders"
            "#,
        );
        let config = load(file.path()).expect("config should parse");
        let conn = config
            .connection
            .expect("connection section must be present");
        assert_eq!(conn.host.as_deref(), Some("myhost.example.com"));
        assert_eq!(conn.dbname.as_deref(), Some("app_myhost.example.com"));
        unsafe { std::env::remove_var("PGCOPY_TEST_HOST") };
    }

    #[test]
    fn connection_env_var_not_set_yields_none() {
        // Убедимся, что переменная не установлена.
        unsafe { std::env::remove_var("PGCOPY_TEST_NOT_SET") };
        let file = write_temp_config(
            r#"
            [general]
            compression = "zstd"

            [connection]
            host = "{PGCOPY_TEST_NOT_SET}"
            dbname = "mydb"

            [[objects]]
            select = "select * from public.orders"
            "#,
        );
        let config = load(file.path()).expect("config should parse");
        let conn = config
            .connection
            .expect("connection section must be present");
        assert!(conn.host.is_none(), "host should be none for unset env var");
        assert_eq!(conn.dbname.as_deref(), Some("mydb"));
    }

    #[test]
    fn connection_env_var_port_from_string() {
        unsafe { std::env::set_var("PGCOPY_TEST_PORT_STR", "6543") };
        let file = write_temp_config(
            r#"
            [general]
            compression = "zstd"

            [connection]
            port = "{PGCOPY_TEST_PORT_STR}"

            [[objects]]
            select = "select * from public.orders"
            "#,
        );
        let config = load(file.path()).expect("config should parse");
        let conn = config
            .connection
            .expect("connection section must be present");
        assert_eq!(conn.port.map(NonZeroU16::get), Some(6543));
        unsafe { std::env::remove_var("PGCOPY_TEST_PORT_STR") };
    }

    #[test]
    fn connection_env_var_port_not_set_yields_none() {
        unsafe { std::env::remove_var("PGCOPY_TEST_PORT_MISSING") };
        let file = write_temp_config(
            r#"
            [general]
            compression = "zstd"

            [connection]
            port = "{PGCOPY_TEST_PORT_MISSING}"
            dbname = "mydb"

            [[objects]]
            select = "select * from public.orders"
            "#,
        );
        let config = load(file.path()).expect("config should parse");
        let conn = config
            .connection
            .expect("connection section must be present");
        assert!(conn.port.is_none(), "port should be none for unset env var");
        assert_eq!(conn.dbname.as_deref(), Some("mydb"));
    }
}

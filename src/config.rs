use std::{num::NonZeroU16, num::NonZeroUsize, path::Path};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::env_dsn::ConnectionOverrides;
use crate::retry::{RetryOptions, RetryPolicy};
use crate::scratch::TempCompression;
use crate::select_dsl::{ProjectionKind, SelectDsl};
use crate::sql::{Identifier, quoted_fq_name};
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
    /// Сжатие временных COPY-файлов до сборки bundle.
    pub(crate) temp_compression: TempCompression,
    /// Политика повторных попыток экспорта одного объекта.
    pub(crate) retry: RetryPolicy,
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
    /// Распарсенная форма DSL с исходной строкой для ошибок и дебага.
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
        let select_raw = format!("select * from {}", quoted_fq_name(schema, name));
        let select = SelectDsl::parse(&select_raw).with_context(|| {
            format!("failed to build dependency select for source relation {schema}.{name}")
        })?;
        Ok(Self {
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
        self.resolve_with_env(|name| std::env::var(name).unwrap_or_default())
    }

    fn resolve_with_env(
        &self,
        mut env_value: impl FnMut(&str) -> String,
    ) -> Result<Option<ConnectionOverrides>> {
        let mut resolve = |value: Option<&str>| {
            value
                .map(|value| interpolate_env(value, &mut env_value))
                .transpose()
                .map(Option::flatten)
        };
        let host = resolve(self.host.as_deref())?;

        let port = match &self.port {
            Some(PortValue::Integer(p)) => Some(
                NonZeroU16::new(*p)
                    .ok_or_else(|| anyhow::anyhow!("connection.port must be non-zero"))?,
            ),
            Some(PortValue::String(s)) => resolve(Some(s))?
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

        let dbname = resolve(self.dbname.as_deref())?;
        let user = resolve(self.user.as_deref())?;
        let password = resolve(self.password.as_deref())?;

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
    #[serde(default)]
    temp_compression: TempCompression,
    #[serde(flatten)]
    retry: RetryOptions,
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
            temp_compression: TempCompression::default(),
            retry: RetryOptions::default(),
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
fn interpolate_env(
    value: &str,
    mut env_value: impl FnMut(&str) -> String,
) -> Result<Option<String>> {
    let mut result = String::with_capacity(value.len());
    let mut rest = value;
    while let Some((before, after)) = rest.split_once('{') {
        result.push_str(before);
        let Some((name, after)) = after.split_once('}') else {
            bail!("unclosed '{{' in connection config value '{value}'");
        };
        result.push_str(&env_value(name));
        rest = after;
    }
    result.push_str(rest);

    let trimmed_len = result.trim_end().len();
    result.truncate(trimmed_len);
    let leading_ws = result.len() - result.trim_start().len();
    if leading_ws != 0 {
        result.drain(..leading_ws);
    }
    Ok((!result.is_empty()).then_some(result))
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
            temp_compression: parsed.general.temp_compression,
            retry: parsed.general.retry.resolve(RetryPolicy::default()),
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
    if select.has_clauses() {
        bail!(
            "config validation error: objects[{index}] export_as='view' does not allow WHERE/ORDER BY/LIMIT"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{io::Write, num::NonZeroU16};

    use super::{Config, ExportAs, RawConnection, interpolate_env, load};

    fn load_config(raw: &str) -> anyhow::Result<Config> {
        let mut file = tempfile::NamedTempFile::new().expect("temp file must be created");
        file.write_all(raw.as_bytes())
            .expect("config file must be written");
        load(file.path())
    }

    fn connection_config(raw: &str) -> anyhow::Result<Config> {
        load_config(&format!(
            "[connection]\n{raw}\n[[objects]]\nselect = 'select * from public.orders'"
        ))
    }

    #[test]
    fn applies_defaults_without_connection() {
        let config = load_config("[[objects]]\nselect = 'select * from public.orders'")
            .expect("config should parse");
        assert_eq!(config.objects.len(), 1);
        assert_eq!(config.objects[0].export_as, ExportAs::Table);
        assert!(config.connection.is_none());
        assert_eq!(
            config.general.temp_compression,
            crate::scratch::TempCompression::Zstd
        );
        assert_eq!(config.general.retry.attempts.get(), 3);
        assert_eq!(
            config.general.retry.delay,
            std::time::Duration::from_secs(1)
        );
        assert_eq!(config.general.retry.timeout, None);
        assert!(
            connection_config("")
                .expect("empty connection must parse")
                .connection
                .is_none()
        );
    }

    #[test]
    fn parses_temporary_compression_and_rejects_unknown_modes() {
        for (value, expected) in [
            ("none", crate::scratch::TempCompression::None),
            ("zstd", crate::scratch::TempCompression::Zstd),
        ] {
            let config = load_config(&format!(
                "[general]\ntemp_compression = '{value}'\n[[objects]]\nselect = 'select * from public.orders'"
            )).unwrap();
            assert_eq!(config.general.temp_compression, expected);
        }
        assert!(load_config("[general]\ntemp_compression = 'gzip'\n[[objects]]\nselect = 'select * from public.orders'").is_err());
    }

    #[test]
    fn parses_retry_settings_and_rejects_invalid_values() {
        let config = load_config("[general]\nattempts = 4\nretry_delay_ms = 25\nattempt_timeout_ms = 200\n[[objects]]\nselect = 'select * from public.orders'").expect("retry config must parse");
        assert_eq!(config.general.retry.attempts.get(), 4);
        assert_eq!(
            config.general.retry.delay,
            std::time::Duration::from_millis(25)
        );
        assert_eq!(
            config.general.retry.timeout,
            Some(std::time::Duration::from_millis(200))
        );
        for setting in [
            "attempts = 0",
            "attempts = -1",
            "attempts = 'three'",
            "retry_delay_ms = -1",
            "attempt_timeout_ms = -1",
            "attempt_timeout_ms = 4294967296",
        ] {
            assert!(
                load_config(&format!(
                    "[general]\n{setting}\n[[objects]]\nselect = 'select * from public.orders'"
                ))
                .is_err(),
                "must reject {setting}"
            );
        }
    }

    #[test]
    fn rejects_view_export_with_clauses_or_projection() {
        for (select, message) in [
            (
                "select * from reporting.v_sales where id > 10",
                "does not allow WHERE/ORDER BY/LIMIT",
            ),
            (
                "select id from reporting.v_sales",
                "requires 'select * from schema.object'",
            ),
        ] {
            let error = load_config(&format!(
                "[[objects]]\nselect = '{select}'\nexport_as = 'view'"
            ))
            .expect_err("invalid view config must fail");
            assert!(error.to_string().contains(message), "{error}");
        }
    }

    #[test]
    fn resolves_explicit_connection_values() {
        let config = connection_config(
            "host = 'db.example.com'\nport = 5433\ndbname = 'mydb'\nuser = 'myuser'\npassword = 'secret'",
        )
        .expect("config should parse");
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
    fn resolves_partial_connection_values() {
        let config = connection_config("host = '  db.example.com  '\ndbname = 'mydb'")
            .expect("config should parse");
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
    fn accepts_integer_and_string_ports_and_rejects_zero() {
        for port in ["5433", "'5433'"] {
            let conn = connection_config(&format!("port = {port}"))
                .expect("port must parse")
                .connection
                .expect("connection must be present");
            assert_eq!(conn.port.map(NonZeroU16::get), Some(5433));
        }
        for port in ["0", "'0'"] {
            let error =
                connection_config(&format!("port = {port}")).expect_err("zero port must fail");
            assert!(
                error
                    .to_string()
                    .contains("connection.port must be non-zero")
            );
        }
    }

    #[test]
    fn resolves_environment_values_and_omits_missing_values() {
        let raw: RawConnection = toml::from_str(
            "host = '{HOST}'\nport = '{PORT}'\ndbname = 'app_{HOST}'\nuser = '{MISSING}'",
        )
        .expect("connection must parse");
        let conn = raw
            .resolve_with_env(|name| match name {
                "HOST" => "myhost.example.com".to_owned(),
                "PORT" => "6543".to_owned(),
                _ => String::new(),
            })
            .expect("connection must resolve")
            .expect("connection must be present");
        assert_eq!(conn.host.as_deref(), Some("myhost.example.com"));
        assert_eq!(conn.port.map(NonZeroU16::get), Some(6543));
        assert_eq!(conn.dbname.as_deref(), Some("app_myhost.example.com"));
        assert!(conn.user.is_none());
        assert!(conn.password.is_none());

        let raw: RawConnection =
            toml::from_str("host = '{HOST}'\nport = '{PORT}'").expect("connection must parse");
        assert!(
            raw.resolve_with_env(|_| String::new())
                .expect("missing values must resolve")
                .is_none()
        );
    }

    #[test]
    fn interpolation_preserves_literals_and_trims_only_result() {
        for (input, expected) in [
            ("  хост  ", Some("хост")),
            ("  {A}  ", Some("значение")),
            ("a{A}{MISSING}}z", Some("a значение }z")),
            ("{MISSING}", None),
            ("   ", None),
            ("", None),
        ] {
            let value = interpolate_env(input, |name| {
                if name == "A" {
                    " значение ".to_owned()
                } else {
                    String::new()
                }
            })
            .expect("interpolation must succeed");
            assert_eq!(value.as_deref(), expected, "input: {input:?}");
        }
        let error = interpolate_env("prefix{UNCLOSED", |_| String::new())
            .expect_err("unclosed interpolation must fail");
        assert!(error.to_string().contains("unclosed '{'"));
    }
}

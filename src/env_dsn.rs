use anyhow::{Result, bail};
use std::env;
use tokio_postgres::Config;

/// CLI-переопределения параметров подключения к PostgreSQL.
///
/// Приоритет источников для каждого поля: CLI -> переменная окружения -> дефолт (если есть).
#[derive(Debug, Clone, Default)]
pub struct ConnectionOverrides {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub dbname: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
}

/// Строит конфигурацию подключения к source базе.
pub fn source_config(overrides: &ConnectionOverrides) -> Result<Config> {
    build_default(overrides)
}

/// Строит конфигурацию подключения к target базе.
pub fn target_config(overrides: &ConnectionOverrides) -> Result<Config> {
    build_default(overrides)
}

fn build_default(overrides: &ConnectionOverrides) -> Result<Config> {
    let host = resolve_required_string(overrides.host.as_deref(), "PGHOST", "--host")?;
    let port = resolve_port(overrides.port)?;
    let dbname = resolve_required_string(overrides.dbname.as_deref(), "PGDATABASE", "--dbname")?;
    let user = resolve_required_string(overrides.user.as_deref(), "PGUSER", "--username")?;
    let password = resolve_optional_string(overrides.password.as_deref(), "PGPASSWORD")?;

    let mut config = Config::new();
    config.host(&host);
    config.port(port);
    config.dbname(&dbname);
    config.user(&user);
    if let Some(password) = password.as_deref() {
        config.password(password);
    }

    Ok(config)
}

fn resolve_required_string(
    cli_value: Option<&str>,
    env_name: &str,
    cli_flag: &str,
) -> Result<String> {
    if let Some(value) = normalize_non_empty(cli_value) {
        return Ok(value);
    }

    if let Some(value) = normalize_non_empty(read_env(env_name)?.as_deref()) {
        return Ok(value);
    }

    bail!("missing PostgreSQL connection parameter: set {cli_flag} or {env_name}")
}

fn resolve_optional_string(cli_value: Option<&str>, env_name: &str) -> Result<Option<String>> {
    if let Some(value) = normalize_non_empty(cli_value) {
        return Ok(Some(value));
    }

    Ok(normalize_non_empty(read_env(env_name)?.as_deref()))
}

fn resolve_port(cli_port: Option<u16>) -> Result<u16> {
    if let Some(port) = cli_port {
        return Ok(port);
    }

    // Держим поведение, совместимое с libpq: порт по умолчанию 5432.
    let Some(raw_port) = read_env("PGPORT")? else {
        return Ok(5432);
    };

    let trimmed = raw_port.trim();
    if trimmed.is_empty() {
        return Ok(5432);
    }

    let parsed = trimmed.parse::<u16>().map_err(|_| {
        anyhow::anyhow!("invalid PGPORT value '{trimmed}', expected integer in 1..65535")
    })?;

    if parsed == 0 {
        bail!("invalid PGPORT value '0', expected integer in 1..65535");
    }

    Ok(parsed)
}

fn normalize_non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn read_env(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => {
            bail!("environment variable {name} contains non-Unicode data")
        }
    }
}

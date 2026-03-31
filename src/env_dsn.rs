use anyhow::{Result, bail};
use std::env;
use std::num::NonZeroU16;
use tokio_postgres::Config;

/// CLI-переопределения параметров подключения к `PostgreSQL`.
///
/// Приоритет источников для каждого поля: CLI -> переменная окружения -> дефолт (если есть).
#[derive(Debug, Clone, Default)]
pub struct ConnectionOverrides {
    pub host: Option<String>,
    pub port: Option<NonZeroU16>,
    pub dbname: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
}

/// Строит конфигурацию подключения к `PostgreSQL`.
pub fn config(overrides: &ConnectionOverrides) -> Result<Config> {
    build_default(overrides)
}

fn build_default(overrides: &ConnectionOverrides) -> Result<Config> {
    let host = resolve_string(overrides.host.as_deref(), "PGHOST")?;
    let port = resolve_port(overrides.port)?;
    let dbname = resolve_string(overrides.dbname.as_deref(), "PGDATABASE")?;
    let user = resolve_string(overrides.user.as_deref(), "PGUSER")?;
    let password = resolve_string(overrides.password.as_deref(), "PGPASSWORD")?;

    let mut missing_required = Vec::new();
    if host.is_none() {
        missing_required.push("--host/PGHOST");
    }
    if dbname.is_none() {
        missing_required.push("--dbname/PGDATABASE");
    }
    if user.is_none() {
        missing_required.push("--username/PGUSER");
    }

    if !missing_required.is_empty() {
        bail!(
            "missing PostgreSQL connection parameters: {} (set corresponding CLI flags or env variables)",
            missing_required.join(", ")
        );
    }

    let (Some(host), Some(dbname), Some(user)) = (host, dbname, user) else {
        unreachable!("required connection parameters must be resolved after missing check");
    };

    let mut config = Config::new();
    config.host(&host);
    config.port(port.get());
    config.dbname(&dbname);
    config.user(&user);
    if let Some(password) = password.as_deref() {
        config.password(password);
    }

    Ok(config)
}

fn resolve_string(cli_value: Option<&str>, env_name: &str) -> Result<Option<String>> {
    if let Some(value) = normalize_non_empty(cli_value) {
        return Ok(Some(value));
    }

    Ok(normalize_non_empty(read_env(env_name)?.as_deref()))
}

fn resolve_port(cli_port: Option<NonZeroU16>) -> Result<NonZeroU16> {
    if let Some(port) = cli_port {
        return Ok(port);
    }

    // Держим поведение, совместимое с libpq: порт по умолчанию 5432.
    let Some(raw_port) = read_env("PGPORT")? else {
        return Ok(default_port());
    };

    let trimmed = raw_port.trim();
    if trimmed.is_empty() {
        return Ok(default_port());
    }

    trimmed.parse::<NonZeroU16>().map_err(|_| {
        anyhow::anyhow!("invalid PGPORT value '{trimmed}', expected integer in 1..65535")
    })
}

fn default_port() -> NonZeroU16 {
    // compile-time constant in practice; wrapped in fn to keep call sites explicit.
    NonZeroU16::new(5432).expect("default PostgreSQL port must be non-zero")
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

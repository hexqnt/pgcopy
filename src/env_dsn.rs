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
///
/// Приоритет для каждого параметра: CLI → переменная окружения → дефолт.
/// Для пароля дополнительно проверяется `~/.pgpass` (низший приоритет).
pub(crate) fn build(overrides: &ConnectionOverrides) -> Result<Config> {
    let host = resolve_string(overrides.host.as_deref(), "PGHOST")?;
    let port = resolve_port(overrides.port)?;
    let dbname = resolve_string(overrides.dbname.as_deref(), "PGDATABASE")?;
    let user = resolve_string(overrides.user.as_deref(), "PGUSER")?;
    let password = resolve_string(overrides.password.as_deref(), "PGPASSWORD")?;

    // Если PGDATABASE/PGUSER не заданы — используем имя пользователя ОС (поведение libpq).
    // Если и его не удалось определить — fallback на "postgres".
    let os_user = os_user_name();
    let dbname = dbname
        .or_else(|| os_user.clone())
        .or_else(|| Some(String::from("postgres")));
    let user = user.or(os_user).or_else(|| Some(String::from("postgres")));

    // Если пароль не задан ни через CLI, ни через PGPASSWORD — пробуем .pgpass.
    let password = password.or_else(|| {
        crate::pgpass::lookup(host.as_deref(), port, dbname.as_deref(), user.as_deref())
    });

    let mut config = Config::new();
    config.port(port.get());

    // PGHOST не задаём при отсутствии: tokio-postgres использует Unix-сокет (как libpq).
    if let Some(host) = host.as_deref() {
        config.host(host);
    }
    if let Some(dbname) = dbname.as_deref() {
        config.dbname(dbname);
    }
    if let Some(user) = user.as_deref() {
        config.user(user);
    }
    if let Some(password) = password.as_deref() {
        config.password(password);
    }

    Ok(config)
}

/// Возвращает имя текущего пользователя ОС.
///
/// На Unix: читает `$USER`; на Windows: `$USERNAME`, затем `$USER`.
/// Если переменная не установлена или пуста — возвращает `None`
/// (fallback на `"postgres"` происходит выше, в `build`).
fn os_user_name() -> Option<String> {
    #[cfg(unix)]
    {
        std::env::var("USER").ok()
    }
    #[cfg(not(unix))]
    {
        std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .ok()
    }
    .map(|s| s.trim().to_owned())
    .filter(|s| !s.is_empty())
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

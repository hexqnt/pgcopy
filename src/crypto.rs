use std::env;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};

const AGE_MAGIC_PREFIX: &[u8] = b"age-encryption.org/v1";

/// Невалидный (пустой) пароль не допускается на уровне типа.
#[derive(Debug, Clone)]
pub struct BundlePassword(String);

impl BundlePassword {
    fn parse(value: &str, source: &str) -> Result<Self> {
        if value.is_empty() {
            bail!("{source} must not be empty");
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Возвращает пароль для bundle из CLI или окружения.
///
/// Приоритет: `--password` -> `PASSWORD`.
pub fn resolve_bundle_password(cli_password: Option<&str>) -> Result<Option<BundlePassword>> {
    if let Some(password) = cli_password {
        return BundlePassword::parse(password, "--password").map(Some);
    }

    match env::var("PASSWORD") {
        Ok(value) if !value.is_empty() => BundlePassword::parse(&value, "PASSWORD").map(Some),
        Ok(_) | Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => {
            bail!("environment variable 'PASSWORD' contains non-Unicode data")
        }
    }
}

/// Быстрая проверка, что файл похож на age-encrypted bundle.
///
/// Проверка опирается только на префикс заголовка и не валидирует весь файл.
pub fn is_age_encrypted_bundle(path: &Path) -> Result<bool> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open bundle {}", path.display()))?;
    let mut header = [0_u8; 64];
    let read = file
        .read(&mut header)
        .with_context(|| format!("failed to read bundle header {}", path.display()))?;

    Ok(header[..read].starts_with(AGE_MAGIC_PREFIX))
}

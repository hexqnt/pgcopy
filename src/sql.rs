use std::fmt;

use anyhow::{Result, bail};

/// Строго валидированный SQL-идентификатор (ASCII letters/digits/_).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Identifier(String);

impl Identifier {
    /// Парсит идентификатор и гарантирует базовые инварианты формата.
    pub fn parse(value: &str, field: &str) -> Result<Self> {
        if !is_valid_identifier(value) {
            bail!(
                "invalid {field} identifier '{value}': only ASCII letters/digits/underscore are supported"
            );
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Identifier {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for Identifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Проверяет, что идентификатор состоит только из ASCII-букв/цифр и `_`.
pub fn is_valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Экранирует PostgreSQL-идентификатор двойными кавычками.
pub fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// Формирует fully-qualified имя `schema.name` c корректным quoting.
pub fn quoted_fq_name(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

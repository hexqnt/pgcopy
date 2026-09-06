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
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Экранирует PostgreSQL-идентификатор двойными кавычками.
pub fn quote_ident(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    push_quoted_ident(&mut quoted, value);
    quoted
}

/// Формирует fully-qualified имя `schema.name` c корректным quoting.
pub fn quoted_fq_name(schema: &str, name: &str) -> String {
    let mut quoted = String::with_capacity(schema.len() + name.len() + 5);
    push_quoted_ident(&mut quoted, schema);
    quoted.push('.');
    push_quoted_ident(&mut quoted, name);
    quoted
}

fn push_quoted_ident(output: &mut String, value: &str) {
    output.push('"');
    for part in value.split_inclusive('"') {
        output.push_str(part);
        if part.ends_with('"') {
            output.push('"');
        }
    }
    output.push('"');
}

#[cfg(test)]
mod tests {
    use super::{quote_ident, quoted_fq_name};

    #[test]
    fn quotes_identifiers_with_embedded_quotes_and_unicode() {
        assert_eq!(quote_ident(""), "\"\"");
        assert_eq!(quote_ident("a\"\"б\""), "\"a\"\"\"\"б\"\"\"");
        assert_eq!(
            quoted_fq_name("схема\"", "имя.таблицы"),
            "\"схема\"\"\".\"имя.таблицы\""
        );
    }
}

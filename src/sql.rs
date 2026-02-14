use anyhow::{Result, bail};

/// Проверяет, что идентификатор состоит только из ASCII-букв/цифр и `_`.
pub fn is_valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Возвращает ошибку, если идентификатор не проходит базовую валидацию.
pub fn ensure_identifier(value: &str, field: &str) -> Result<()> {
    if !is_valid_identifier(value) {
        bail!(
            "invalid {field} identifier '{value}': only ASCII letters/digits/underscore are supported"
        );
    }
    Ok(())
}

/// Экранирует PostgreSQL-идентификатор двойными кавычками.
pub fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// Формирует fully-qualified имя `schema.name` c корректным quoting.
pub fn quoted_fq_name(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

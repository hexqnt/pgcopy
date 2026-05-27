//! Поиск пароля в стандартном файле `.pgpass` PostgreSQL.
//!
//! Формат: `hostname:port:database:username:password`.
//! `*` — wildcard, `\:` и `\\` — escape, `#` — комментарий.

use std::num::NonZeroU16;
use std::path::PathBuf;

/// Ищет пароль в `.pgpass` для заданных параметров подключения.
///
/// Если хост не задан (Unix-сокет), для сопоставления используется `localhost`.
pub(crate) fn lookup(
    host: Option<&str>,
    port: NonZeroU16,
    dbname: Option<&str>,
    user: Option<&str>,
) -> Option<String> {
    let pgpass_path = file_path()?;
    let contents = std::fs::read_to_string(&pgpass_path).ok()?;

    let host = host.unwrap_or("localhost");
    let port_str = port.get().to_string();
    let dbname = dbname.unwrap_or("");
    let user = user.unwrap_or("");

    for line in contents.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Some(entry) = PgPassEntry::parse(line) else {
            continue;
        };
        if entry.matches(host, &port_str, dbname, user) {
            return Some(entry.password);
        }
    }

    None
}

/// Одна разобранная запись `.pgpass`.
#[derive(Debug, PartialEq, Eq)]
struct PgPassEntry {
    host: String,
    port: String,
    dbname: String,
    user: String,
    password: String,
}

impl PgPassEntry {
    fn parse(line: &str) -> Option<Self> {
        let mut fields = parse_line(line);
        if fields.len() != 5 {
            return None;
        }

        let password = fields.pop()?;
        let user = fields.pop()?;
        let dbname = fields.pop()?;
        let port = fields.pop()?;
        let host = fields.pop()?;

        Some(Self {
            host,
            port,
            dbname,
            user,
            password,
        })
    }

    fn matches(&self, host: &str, port: &str, dbname: &str, user: &str) -> bool {
        field_matches(&self.host, host)
            && field_matches(&self.port, port)
            && field_matches(&self.dbname, dbname)
            && field_matches(&self.user, user)
    }
}

/// Возвращает пароль из `.pgpass`-строки, если она подходит подключению.
#[cfg(test)]
fn lookup_line(line: &str, host: &str, port: &str, dbname: &str, user: &str) -> Option<String> {
    let entry = PgPassEntry::parse(line)?;
    if entry.matches(host, port, dbname, user) {
        Some(entry.password)
    } else {
        None
    }
}

/// Путь к `.pgpass` в зависимости от платформы.
fn file_path() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        let home = std::env::var("HOME").ok()?;
        Some(PathBuf::from(home).join(".pgpass"))
    }
    #[cfg(windows)]
    {
        let appdata = std::env::var("APPDATA").ok()?;
        Some(PathBuf::from(appdata).join("postgres").join("pgpass.conf"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// Разбирает строку `.pgpass` на поля с учётом escape-последовательностей.
fn parse_line(line: &str) -> Vec<String> {
    let mut fields = Vec::with_capacity(5);
    let mut current = String::new();
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                if let Some(&next) = chars.peek() {
                    current.push(next);
                    chars.next();
                } else {
                    current.push('\\');
                }
            }
            ':' => {
                fields.push(std::mem::take(&mut current));
                if fields.len() == 4 {
                    // Всё оставшееся — пароль (может содержать `:` и escape).
                    let mut rest = String::new();
                    while let Some(ch) = chars.next() {
                        if ch == '\\' {
                            if let Some(&next) = chars.peek() {
                                rest.push(next);
                                chars.next();
                            } else {
                                rest.push('\\');
                            }
                        } else {
                            rest.push(ch);
                        }
                    }
                    fields.push(rest);
                    return fields;
                }
            }
            _ => current.push(ch),
        }
    }
    fields.push(current);
    fields
}

/// Проверяет совпадение поля `.pgpass` с реальным значением.
fn field_matches(pattern: &str, actual: &str) -> bool {
    pattern == "*" || pattern == actual
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_line() {
        let fields = parse_line("localhost:5432:mydb:myuser:mypass");
        assert_eq!(
            fields,
            vec!["localhost", "5432", "mydb", "myuser", "mypass"]
        );
    }

    #[test]
    fn parse_with_wildcards() {
        let fields = parse_line("*:*:*:*:secret");
        assert_eq!(fields, vec!["*", "*", "*", "*", "secret"]);
    }

    #[test]
    fn parse_with_escaped_colon_in_password() {
        let fields = parse_line(r"localhost:5432:mydb:myuser:pass\:word");
        assert_eq!(
            fields,
            vec!["localhost", "5432", "mydb", "myuser", "pass:word"]
        );
    }

    #[test]
    fn parse_with_escaped_backslash() {
        let fields = parse_line(r"localhost:5432:mydb:myuser:pass\\word");
        assert_eq!(
            fields,
            vec!["localhost", "5432", "mydb", "myuser", r"pass\word"]
        );
    }

    #[test]
    fn parse_with_escaped_colon_in_host() {
        let fields = parse_line(r"host\:name:5432:mydb:myuser:secret");
        assert_eq!(
            fields,
            vec!["host:name", "5432", "mydb", "myuser", "secret"]
        );
    }

    #[test]
    fn lookup_line_preserves_password_spaces() {
        let password = lookup_line(
            "localhost:5432:mydb:myuser:  secret  ",
            "localhost",
            "5432",
            "mydb",
            "myuser",
        );
        assert_eq!(password.as_deref(), Some("  secret  "));
    }

    #[test]
    fn lookup_line_rejects_mismatched_connection() {
        let password = lookup_line(
            "localhost:5432:mydb:myuser:secret",
            "localhost",
            "5432",
            "otherdb",
            "myuser",
        );
        assert!(password.is_none());
    }

    #[test]
    fn field_exact_match() {
        assert!(field_matches("localhost", "localhost"));
        assert!(!field_matches("localhost", "otherhost"));
    }

    #[test]
    fn field_wildcard_match() {
        assert!(field_matches("*", "localhost"));
        assert!(field_matches("*", "anything"));
    }
}

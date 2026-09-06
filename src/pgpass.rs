//! Поиск пароля в стандартном файле `.pgpass` PostgreSQL.
//!
//! Формат: `hostname:port:database:username:password`.
//! `*` — wildcard, `\:` и `\\` — escape, `#` — комментарий.

use std::num::NonZeroU16;
use std::path::PathBuf;

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
        let [host, port, dbname, user, password] = parse_line(line)?;

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
fn parse_line(line: &str) -> Option<[String; 5]> {
    let mut fields = std::array::from_fn(|_| String::new());
    let mut field_index = 0;
    let mut chars = line.chars();

    while let Some(ch) = chars.next() {
        match ch {
            '\\' => fields[field_index].push(chars.next().unwrap_or('\\')),
            ':' if field_index < fields.len() - 1 => field_index += 1,
            _ => fields[field_index].push(ch),
        }
    }

    (field_index == fields.len() - 1).then_some(fields)
}

/// Проверяет совпадение поля `.pgpass` с реальным значением.
fn field_matches(pattern: &str, actual: &str) -> bool {
    pattern == "*" || pattern == actual
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_incomplete_entries() {
        for line in [
            "",
            "host",
            "host:5432:db:user",
            r"host:5432:db:user\:password",
        ] {
            assert!(parse_line(line).is_none(), "unexpected entry: {line}");
        }
    }

    #[test]
    fn preserves_password_colons_and_trailing_backslash() {
        let fields = parse_line("host:5432:db:user:pass:word\\")
            .expect("must parse password containing a colon and trailing backslash");
        assert_eq!(fields[4], "pass:word\\");
        assert_eq!(
            parse_line("::::"),
            Some(std::array::from_fn(|_| String::new()))
        );
    }

    #[test]
    fn parse_basic_line() {
        let fields =
            parse_line("localhost:5432:mydb:myuser:mypass").expect("must parse five fields");
        assert_eq!(fields, ["localhost", "5432", "mydb", "myuser", "mypass"]);
    }

    #[test]
    fn parse_with_wildcards() {
        let fields = parse_line("*:*:*:*:secret").expect("must parse five fields");
        assert_eq!(fields, ["*", "*", "*", "*", "secret"]);
    }

    #[test]
    fn parse_with_escaped_colon_in_password() {
        let fields =
            parse_line(r"localhost:5432:mydb:myuser:pass\:word").expect("must parse five fields");
        assert_eq!(fields, ["localhost", "5432", "mydb", "myuser", "pass:word"]);
    }

    #[test]
    fn parse_with_escaped_backslash() {
        let fields =
            parse_line(r"localhost:5432:mydb:myuser:pass\\word").expect("must parse five fields");
        assert_eq!(
            fields,
            ["localhost", "5432", "mydb", "myuser", r"pass\word"]
        );
    }

    #[test]
    fn parse_with_escaped_colon_in_host() {
        let fields =
            parse_line(r"host\:name:5432:mydb:myuser:secret").expect("must parse five fields");
        assert_eq!(fields, ["host:name", "5432", "mydb", "myuser", "secret"]);
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

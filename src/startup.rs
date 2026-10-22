use std::io::IsTerminal;
use std::path::Path;
use std::{fmt, fmt::Write as _};

use tokio_postgres::{Config as PgConfig, config::Host};

use crate::{describe, importer, info};

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

pub(crate) fn print_startup_banner() -> bool {
    let title = format!("pgcopy v{APP_VERSION}");
    let subtitle = "PostgreSQL transfer and schema context";
    let color = stderr_supports_color();

    if color {
        eprintln!("\x1b[1;36m==>\x1b[0m \x1b[1m{title}\x1b[0m \x1b[2m| {subtitle}\x1b[0m");
    } else {
        eprintln!("==> {title} | {subtitle}");
    }

    color
}

pub(crate) fn print_startup_detail(label: &str, value: impl fmt::Display, color: bool) {
    if color {
        eprintln!("\x1b[2m  {label:>11}:\x1b[0m {value}");
    } else {
        eprintln!("  {label:>11}: {value}");
    }
}

pub(crate) fn print_export_startup_details(
    config_path: &Path,
    out_path: &Path,
    source_config: &PgConfig,
    dry_run: bool,
    color: bool,
) {
    print_command("export", dry_run, color);
    print_startup_detail("source_db", format_pg_connection(source_config), color);
    print_startup_detail("config", config_path.display(), color);
    print_startup_detail("out", out_path.display(), color);
}

pub(crate) fn print_describe_startup_details(
    config_path: &Path,
    out_path: Option<&Path>,
    options: describe::Options,
    source_config: &PgConfig,
    dry_run: bool,
    color: bool,
) {
    print_command("describe", dry_run, color);
    print_startup_detail("source_db", format_pg_connection(source_config), color);
    print_startup_detail("config", config_path.display(), color);
    print_startup_detail(
        "out",
        out_path.map_or_else(|| "stdout".to_owned(), |path| path.display().to_string()),
        color,
    );
    print_startup_detail("format", options.output_format, color);
    print_startup_detail("statistics", options.statistics, color);
    print_startup_detail("formatter", options.formatting, color);
}

pub(crate) fn print_import_startup_details(
    input_path: &Path,
    mode: importer::ImportMode,
    concurrency: usize,
    ddl_only: bool,
    target_config: &PgConfig,
    dry_run: bool,
    color: bool,
) {
    print_command("import", dry_run, color);
    print_startup_detail("target_db", format_pg_connection(target_config), color);
    print_startup_detail("bundle", input_path.display(), color);
    print_startup_detail("mode", mode, color);
    print_startup_detail("concurrency", concurrency, color);
    print_startup_detail("ddl_only", yes_no(ddl_only), color);
}

pub(crate) fn print_info_startup_details(
    input_path: &Path,
    format: info::InfoOutputFormat,
    objects: bool,
    color: bool,
) {
    print_startup_detail("command", "info", color);
    print_startup_detail("bundle", input_path.display(), color);
    print_startup_detail("format", format, color);
    print_startup_detail("objects", yes_no(objects), color);
}

pub(crate) fn format_pg_connection(config: &PgConfig) -> String {
    let ports = config.get_ports();
    let hosts = config.get_hosts();
    let mut connection = if hosts.is_empty() {
        format!("localhost:{}", port_for_host(ports, 0))
    } else {
        String::new()
    };
    for (index, host) in hosts.iter().enumerate() {
        if index != 0 {
            connection.push(',');
        }
        match host {
            Host::Tcp(value) => connection.push_str(value),
            #[cfg(unix)]
            Host::Unix(path) => {
                write!(connection, "{}", path.display()).expect("writing to string is infallible");
            }
        }
        write!(connection, ":{}", port_for_host(ports, index))
            .expect("writing to string is infallible");
    }

    write!(
        connection,
        " / db={} / user={}",
        config.get_dbname().unwrap_or("<default>"),
        config.get_user().unwrap_or("<default>"),
    )
    .expect("writing to string is infallible");
    connection
}

fn port_for_host(ports: &[u16], index: usize) -> u16 {
    match ports {
        [] => 5432,
        [port] => *port,
        many @ [first, ..] => many.get(index).copied().unwrap_or(*first),
    }
}

fn print_command(command: &str, dry_run: bool, color: bool) {
    let suffix = if dry_run { " (dry-run)" } else { "" };
    print_startup_detail("command", format_args!("{command}{suffix}"), color);
}

const fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn stderr_supports_color() -> bool {
    std::io::stderr().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").is_ok_and(|term| term != "dumb")
}

#[cfg(test)]
mod tests {
    use super::{PgConfig, format_pg_connection};

    #[test]
    fn connection_summary_preserves_endpoint_defaults_and_order() {
        let mut config = PgConfig::new();
        assert_eq!(
            format_pg_connection(&config),
            "localhost:5432 / db=<default> / user=<default>"
        );
        config
            .host("first")
            .host("second")
            .host("third")
            .port(5433)
            .dbname("app")
            .user("reader");
        assert_eq!(
            format_pg_connection(&config),
            "first:5433,second:5433,third:5433 / db=app / user=reader"
        );
        config.port(5434);
        assert_eq!(
            format_pg_connection(&config),
            "first:5433,second:5434,third:5433 / db=app / user=reader"
        );
    }

    #[cfg(unix)]
    #[test]
    fn connection_summary_preserves_unix_socket_paths() {
        let mut config = PgConfig::new();
        config.host_path("/tmp/сокет").port(5433);
        assert_eq!(
            format_pg_connection(&config),
            "/tmp/сокет:5433 / db=<default> / user=<default>"
        );
    }
}

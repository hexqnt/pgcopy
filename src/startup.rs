use std::fmt;
use std::io::IsTerminal;
use std::path::Path;

use tokio_postgres::{Config as PgConfig, config::Host};

use crate::{importer, info};

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

pub(crate) fn print_startup_banner() -> bool {
    let title = format!("pgcopy v{APP_VERSION}");
    let subtitle = "PostgreSQL bundle export/import";
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
    color: bool,
) {
    print_startup_detail("command", "export", color);
    print_startup_detail("source_db", format_pg_connection(source_config), color);
    print_startup_detail("config", config_path.display(), color);
    print_startup_detail("out", out_path.display(), color);
}

pub(crate) fn print_import_startup_details(
    input_path: &Path,
    mode: importer::ImportMode,
    concurrency: usize,
    ddl_only: bool,
    target_config: &PgConfig,
    color: bool,
) {
    print_startup_detail("command", "import", color);
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

    let endpoints = if hosts.is_empty() {
        vec![format!("localhost:{}", port_for_host(ports, 0))]
    } else {
        hosts
            .iter()
            .enumerate()
            .map(|(index, host)| {
                format!("{}:{}", format_pg_host(host), port_for_host(ports, index))
            })
            .collect::<Vec<_>>()
    };

    format!(
        "{} / db={} / user={}",
        endpoints.join(","),
        config.get_dbname().unwrap_or("<default>"),
        config.get_user().unwrap_or("<default>"),
    )
}

fn format_pg_host(host: &Host) -> String {
    match host {
        Host::Tcp(value) => value.to_owned(),
        #[cfg(unix)]
        Host::Unix(path) => path.display().to_string(),
    }
}

fn port_for_host(ports: &[u16], index: usize) -> u16 {
    match ports {
        [] => 5432,
        [port] => *port,
        many => many.get(index).copied().unwrap_or(many[0]),
    }
}

const fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn stderr_supports_color() -> bool {
    std::io::stderr().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").is_ok_and(|term| term != "dumb")
}

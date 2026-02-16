use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use tokio_postgres::{Config as PgConfig, config::Host};
use tracing_subscriber::filter::Directive;

mod bundle_io;
mod config;
mod crypto;
mod env_dsn;
mod export;
mod importer;
mod info;
mod manifest;
mod pg;
mod progress;
mod select_dsl;
mod sql;
mod types;

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Parser)]
#[command(name = "pgcopy")]
#[command(about = "Export/import selected PostgreSQL objects into a single bundle file")]
#[command(
    long_about = "pgcopy exports selected PostgreSQL tables/materialized views/views into one compressed bundle \
and imports that bundle into another PostgreSQL database."
)]
#[command(
    after_help = "Connection parameters can be provided via CLI flags or PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD.\n\
Bundle encryption password can be provided via --password or environment variable PASSWORD.\n\n\
Run `pgcopy export --help`, `pgcopy import --help`, or `pgcopy info --help` for command-specific examples."
)]
struct Cli {
    #[arg(
        long,
        global = true,
        help = "Suppress non-essential output (startup banner and progress bars)"
    )]
    quiet: bool,
    #[arg(
        long,
        global = true,
        help = "Disable progress bars (useful for CI logs)"
    )]
    no_progress: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Clone, Args, Default)]
struct PgConnectionArgs {
    #[arg(long, value_name = "HOST", help = "PostgreSQL host (fallback: PGHOST)")]
    host: Option<String>,
    #[arg(
        long,
        value_name = "PORT",
        help = "PostgreSQL port (fallback: PGPORT, default: 5432)"
    )]
    port: Option<u16>,
    #[arg(
        long,
        value_name = "DBNAME",
        help = "PostgreSQL database name (fallback: PGDATABASE)"
    )]
    dbname: Option<String>,
    #[arg(
        long = "username",
        visible_alias = "user",
        value_name = "USER",
        help = "PostgreSQL user name (fallback: PGUSER)"
    )]
    username: Option<String>,
    #[arg(
        long = "pgpassword",
        value_name = "PASSWORD",
        help = "PostgreSQL password (fallback: PGPASSWORD)"
    )]
    pgpassword: Option<String>,
}

impl PgConnectionArgs {
    fn into_overrides(self) -> env_dsn::ConnectionOverrides {
        env_dsn::ConnectionOverrides {
            host: self.host,
            port: self.port,
            dbname: self.dbname,
            user: self.username,
            password: self.pgpassword,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Commands {
    #[command(
        about = "Export selected source objects into one bundle",
        long_about = "Reads config TOML, extracts DDL + data from the source PostgreSQL database,\n\
writes them into one tar.zst bundle, and optionally encrypts that bundle with a password.\n\
Export concurrency is resolved with priority: CLI --concurrency > general.concurrency in TOML > PGCOPY_CONCURRENCY env."
    )]
    #[command(after_help = "Example:\n\
  pgcopy export --config ./config.toml --out ./bundle.tar.zst \\\n\
    --host 127.0.0.1 --port 5432 --dbname app_db --username app_user --pgpassword secret \\\n\
    --concurrency 4\n\n\
Encrypted bundle:\n\
  pgcopy export --config ./config.toml --out ./bundle.enc --password strong-passphrase")]
    Export {
        #[arg(
            long,
            value_name = "CONFIG_TOML",
            help = "Path to export configuration file (TOML)"
        )]
        config: PathBuf,
        #[arg(
            long,
            value_name = "BUNDLE_FILE",
            help = "Output bundle file path (for example ./bundle.tar.zst)"
        )]
        out: PathBuf,
        #[arg(
            long,
            visible_alias = "concurency",
            value_name = "N",
            help = "Export concurrency (priority: CLI --concurrency > config general.concurrency > PGCOPY_CONCURRENCY env)"
        )]
        concurrency: Option<usize>,
        #[arg(
            long,
            value_name = "PASSWORD",
            help = "Bundle encryption password (fallback: PASSWORD env)"
        )]
        password: Option<String>,
        #[command(flatten)]
        connection: PgConnectionArgs,
    },
    #[command(
        about = "Import bundle into target PostgreSQL database",
        long_about = "Reads bundle file, validates compatibility, creates target tables, and loads data\n\
into the target PostgreSQL database. Parallel import concurrency is configured via --concurrency."
    )]
    #[command(after_help = "Example:\n\
  pgcopy import --in ./bundle.tar.zst --mode replace \\\n\
    --host 127.0.0.1 --port 5432 --dbname app_db --username app_user --pgpassword secret\n\n\
Encrypted bundle:\n\
  pgcopy import --in ./bundle.enc --mode replace --password strong-passphrase")]
    Import {
        #[arg(
            long = "in",
            value_name = "BUNDLE_FILE",
            help = "Path to input bundle file created by `pgcopy export`"
        )]
        input: PathBuf,
        #[arg(
            long,
            value_name = "MODE",
            default_value = "replace",
            help = "Import strategy for existing target tables"
        )]
        mode: importer::ImportMode,
        #[arg(
            long,
            visible_alias = "concurency",
            value_name = "N",
            default_value_t = 1,
            help = "Import concurrency (number of objects processed in parallel)"
        )]
        concurrency: usize,
        #[arg(
            long,
            value_name = "PASSWORD",
            help = "Bundle decryption password (fallback: PASSWORD env)"
        )]
        password: Option<String>,
        #[command(flatten)]
        connection: PgConnectionArgs,
    },
    #[command(
        about = "Show bundle metadata",
        long_about = "Reads bundle manifest metadata without connecting to PostgreSQL or importing data."
    )]
    #[command(after_help = "Example:\n\
  pgcopy info --in ./bundle.tar.zst\n\
  pgcopy info --in ./bundle.tar.zst --objects\n\
  pgcopy info --in ./bundle.enc --password strong-passphrase --format json")]
    Info {
        #[arg(
            long = "in",
            value_name = "BUNDLE_FILE",
            help = "Path to input bundle file created by `pgcopy export`"
        )]
        input: PathBuf,
        #[arg(
            long,
            value_name = "PASSWORD",
            help = "Bundle decryption password (fallback: PASSWORD env)"
        )]
        password: Option<String>,
        #[arg(
            long,
            value_name = "FORMAT",
            default_value = "text",
            help = "Output format for bundle metadata"
        )]
        format: info::InfoOutputFormat,
        #[arg(long, help = "Include metadata for each bundled object")]
        objects: bool,
    },
}

fn print_startup_banner() -> bool {
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

fn print_startup_detail(label: &str, value: &str, color: bool) {
    if color {
        eprintln!("\x1b[2m  {label:>11}:\x1b[0m {value}");
    } else {
        eprintln!("  {label:>11}: {value}");
    }
}

fn print_export_startup_details(
    config_path: &Path,
    out_path: &Path,
    source_config: &PgConfig,
    color: bool,
) {
    print_startup_detail("command", "export", color);
    print_startup_detail("source_db", &format_pg_connection(source_config), color);
    print_startup_detail("config", &config_path.display().to_string(), color);
    print_startup_detail("out", &out_path.display().to_string(), color);
}

fn print_import_startup_details(
    input_path: &Path,
    mode: importer::ImportMode,
    concurrency: usize,
    target_config: &PgConfig,
    color: bool,
) {
    let mode = match mode {
        importer::ImportMode::Replace => "replace",
        importer::ImportMode::Append => "append",
    };
    print_startup_detail("command", "import", color);
    print_startup_detail("target_db", &format_pg_connection(target_config), color);
    print_startup_detail("bundle", &input_path.display().to_string(), color);
    print_startup_detail("mode", mode, color);
    print_startup_detail("concurrency", &concurrency.to_string(), color);
}

fn print_info_startup_details(
    input_path: &Path,
    format: info::InfoOutputFormat,
    objects: bool,
    color: bool,
) {
    let format = match format {
        info::InfoOutputFormat::Text => "text",
        info::InfoOutputFormat::Json => "json",
    };
    print_startup_detail("command", "info", color);
    print_startup_detail("bundle", &input_path.display().to_string(), color);
    print_startup_detail("format", format, color);
    print_startup_detail("objects", if objects { "yes" } else { "no" }, color);
}

fn format_pg_connection(config: &PgConfig) -> String {
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
    if ports.is_empty() {
        return 5432;
    }

    if ports.len() == 1 {
        return ports[0];
    }

    ports.get(index).copied().unwrap_or(ports[0])
}

fn stderr_supports_color() -> bool {
    std::io::stderr().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").is_ok_and(|term| term != "dumb")
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let progress_enabled = !cli.quiet && !cli.no_progress;
    let startup_color = if !cli.quiet {
        print_startup_banner()
    } else {
        false
    };

    let default_filter = if cli.quiet { "warn" } else { "info" };
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default_filter.to_owned().into())
        .add_directive(
            "tokio_postgres=warn"
                .parse::<Directive>()
                .expect("valid tracing directive"),
        );

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .without_time()
        .init();

    match cli.command {
        Commands::Export {
            config,
            out,
            concurrency,
            password,
            connection,
        } => {
            let dsn_overrides = connection.into_overrides();
            let source_config = env_dsn::source_config(&dsn_overrides)?;
            if !cli.quiet {
                print_export_startup_details(&config, &out, &source_config, startup_color);
            }
            export::run(
                &config,
                &out,
                concurrency,
                password.as_deref(),
                source_config,
                progress_enabled,
            )
            .await?
        }
        Commands::Import {
            input,
            mode,
            concurrency,
            password,
            connection,
        } => {
            let dsn_overrides = connection.into_overrides();
            let target_config = env_dsn::target_config(&dsn_overrides)?;
            if !cli.quiet {
                print_import_startup_details(
                    &input,
                    mode,
                    concurrency,
                    &target_config,
                    startup_color,
                );
            }
            importer::run(
                &input,
                mode,
                concurrency,
                password.as_deref(),
                target_config,
                progress_enabled,
            )
            .await?
        }
        Commands::Info {
            input,
            password,
            format,
            objects,
        } => {
            if !cli.quiet {
                print_info_startup_details(&input, format, objects, startup_color);
            }
            info::run(&input, password.as_deref(), format, objects)?
        }
    }

    Ok(())
}

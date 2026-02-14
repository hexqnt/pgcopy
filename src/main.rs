use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

mod bundle_io;
mod config;
mod crypto;
mod env_dsn;
mod export;
mod importer;
mod info;
mod manifest;
mod pg;
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

#[tokio::main]
async fn main() -> Result<()> {
    eprintln!("pgcopy v{APP_VERSION} starting");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".to_owned().into()),
        )
        .without_time()
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Export {
            config,
            out,
            concurrency,
            password,
            connection,
        } => {
            let dsn_overrides = connection.into_overrides();
            export::run(
                &config,
                &out,
                concurrency,
                password.as_deref(),
                &dsn_overrides,
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
            importer::run(
                &input,
                mode,
                concurrency,
                password.as_deref(),
                &dsn_overrides,
            )
            .await?
        }
        Commands::Info {
            input,
            password,
            format,
            objects,
        } => info::run(&input, password.as_deref(), format, objects)?,
    }

    Ok(())
}

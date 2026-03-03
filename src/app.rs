use anyhow::Result;
use tracing_subscriber::filter::Directive;

use crate::{
    cli::{Cli, Commands},
    env_dsn, export, importer, info, startup,
};

pub(crate) async fn run(cli: Cli) -> Result<()> {
    let progress_enabled = !cli.quiet && !cli.no_progress;
    let startup_color = !cli.quiet && startup::print_startup_banner();
    init_tracing(cli.quiet);

    match cli.command {
        Commands::Export {
            config,
            out,
            concurrency,
            password,
            connection,
        } => {
            let dsn_overrides = connection.into_overrides();
            let source_config = env_dsn::config(&dsn_overrides)?;
            if !cli.quiet {
                startup::print_export_startup_details(&config, &out, &source_config, startup_color);
            }
            export::run(
                &config,
                &out,
                concurrency,
                password.as_deref(),
                source_config,
                progress_enabled,
            )
            .await?;
        }
        Commands::Import {
            input,
            mode,
            concurrency,
            ddl_only,
            password,
            connection,
        } => {
            let dsn_overrides = connection.into_overrides();
            let target_config = env_dsn::config(&dsn_overrides)?;
            if !cli.quiet {
                startup::print_import_startup_details(
                    &input,
                    mode,
                    concurrency.get(),
                    ddl_only,
                    &target_config,
                    startup_color,
                );
            }
            importer::run(
                &input,
                mode,
                concurrency,
                ddl_only,
                password.as_deref(),
                target_config,
                progress_enabled,
            )
            .await?;
        }
        Commands::Info {
            input,
            password,
            format,
            objects,
        } => {
            if !cli.quiet {
                startup::print_info_startup_details(&input, format, objects, startup_color);
            }
            info::run(&input, password.as_deref(), format, objects)?;
        }
    }

    Ok(())
}

fn init_tracing(quiet: bool) {
    let default_filter = if quiet { "warn" } else { "info" };
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
}

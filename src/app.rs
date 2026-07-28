use std::num::NonZeroUsize;
use std::path::Path;

use anyhow::Result;

use crate::{
    bundle_io,
    cli::{Cli, Commands},
    config, crypto, env_dsn, export, importer, info, startup,
};

fn init_tracing(quiet: bool) {
    let default_directive = if quiet { "warn" } else { "info" };
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_directive))
        .add_directive(
            "tokio_postgres=warn"
                .parse()
                .expect("valid tracing directive"),
        );

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .without_time()
        .init();
}

fn print_dry_run_export(
    cfg: &config::Config,
    config_path: &Path,
    out_path: &Path,
    concurrency: Option<NonZeroUsize>,
    bundle_password: Option<&str>,
    source_config: &tokio_postgres::Config,
    color: bool,
) -> Result<()> {
    let concurrency = export::resolve_export_concurrency(concurrency, &cfg.general)?;
    let bundle_password = crypto::resolve_bundle_password(bundle_password)?;

    startup::print_startup_detail("command", "export (dry-run)", color);
    startup::print_startup_detail(
        "source_db",
        startup::format_pg_connection(source_config),
        color,
    );
    startup::print_startup_detail("config", config_path.display(), color);
    startup::print_startup_detail("out", out_path.display(), color);
    startup::print_startup_detail("objects", cfg.objects.len(), color);
    startup::print_startup_detail("concurrency", concurrency, color);
    let encrypted = bundle_password.is_some();
    startup::print_startup_detail("encrypted", if encrypted { "yes" } else { "no" }, color);

    for (i, obj) in cfg.objects.iter().enumerate() {
        let target = obj.target.as_ref().map_or_else(
            || obj.source_label(),
            |t| format!("{}.{}", t.schema.as_str(), t.name.as_str()),
        );
        let export_as = if obj.export_as.as_str() == "table" {
            String::new()
        } else {
            format!(" (export_as={})", obj.export_as)
        };
        startup::print_startup_detail(
            &format!("  [{i}]"),
            format!("{} -> {target}{export_as}", obj.select_raw),
            color,
        );
    }

    Ok(())
}

fn print_dry_run_import(
    bundle_path: &Path,
    mode: importer::ImportMode,
    concurrency: usize,
    ddl_only: bool,
    bundle_password: Option<&str>,
    target_config: &tokio_postgres::Config,
    color: bool,
) -> Result<()> {
    let access = bundle_io::resolve_access(bundle_path, bundle_password)?;
    let manifest = bundle_io::read_manifest_from_bundle(bundle_path, &access)?;

    startup::print_startup_detail("command", "import (dry-run)", color);
    startup::print_startup_detail(
        "target_db",
        startup::format_pg_connection(target_config),
        color,
    );
    startup::print_startup_detail("bundle", bundle_path.display(), color);
    startup::print_startup_detail("mode", mode, color);
    startup::print_startup_detail("concurrency", concurrency, color);
    startup::print_startup_detail("ddl_only", if ddl_only { "yes" } else { "no" }, color);
    startup::print_startup_detail(
        "encrypted",
        if access.is_encrypted { "yes" } else { "no" },
        color,
    );
    startup::print_startup_detail("objects", manifest.objects.len(), color);

    for (i, obj) in manifest.objects.iter().enumerate() {
        startup::print_startup_detail(
            &format!("  [{i}]"),
            format!(
                "{}.{} -> {}.{} ({} cols, rows~{})",
                obj.source_schema,
                obj.source_name,
                obj.target_schema,
                obj.target_name,
                obj.effective_columns.len(),
                obj.row_estimate
                    .map_or_else(|| "?".to_owned(), |v| v.to_string()),
            ),
            color,
        );
    }

    Ok(())
}
pub(crate) async fn run(cli: Cli) -> Result<()> {
    let progress_enabled = !cli.quiet && !cli.no_progress;
    let startup_color = !cli.quiet && startup::print_startup_banner();
    init_tracing(cli.quiet);

    match cli.command {
        Commands::Export {
            config: config_path,
            out,
            concurrency,
            password,
            connection,
        } => {
            let cfg = config::load(&config_path)?;
            let dsn_overrides = connection.into_overrides();
            let source_config = env_dsn::build(&dsn_overrides, cfg.connection.as_ref())?;

            if cli.dry_run {
                print_dry_run_export(
                    &cfg,
                    &config_path,
                    &out,
                    concurrency,
                    password.as_deref(),
                    &source_config,
                    startup_color,
                )?;
                return Ok(());
            }

            if !cli.quiet {
                startup::print_export_startup_details(
                    &config_path,
                    &out,
                    &source_config,
                    startup_color,
                );
            }
            export::run(
                &config_path,
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
            let target_config = env_dsn::build(&dsn_overrides, None)?;

            if cli.dry_run {
                print_dry_run_import(
                    &input,
                    mode,
                    concurrency.get(),
                    ddl_only,
                    password.as_deref(),
                    &target_config,
                    startup_color,
                )?;
                return Ok(());
            }

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


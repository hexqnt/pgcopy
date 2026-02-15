use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;

use crate::bundle_io;
use crate::env_dsn;
use crate::pg;

mod compat;
mod copy_stream;
mod load;
mod parallel;
mod progress;
mod replace_tx;
mod sequences;
mod stream;
mod target_table;

/// Режим импорта при наличии объекта в target.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ImportMode {
    #[value(help = "Drop and recreate target table before loading data")]
    Replace,
    #[value(help = "Keep existing target table and append data after compatibility checks")]
    Append,
}

/// Выполняет импорт bundle в target PostgreSQL.
pub async fn run(
    bundle_path: &Path,
    mode: ImportMode,
    concurrency: usize,
    bundle_password: Option<&str>,
    dsn_overrides: &env_dsn::ConnectionOverrides,
    progress_enabled: bool,
) -> Result<()> {
    if concurrency == 0 {
        bail!("import concurrency must be >= 1");
    }

    let access = bundle_io::resolve_access(bundle_path, bundle_password)?;
    let target_config = env_dsn::target_config(dsn_overrides)?;
    let client = pg::connect(&target_config).await?;
    let target_version_num = pg::server_version_num(&client).await?;

    if concurrency == 1 {
        stream::import_objects_streaming(
            bundle_path,
            access.password.as_deref(),
            access.is_encrypted,
            &client,
            mode,
            target_version_num,
            progress_enabled,
        )
        .await?;
        return Ok(());
    }

    let scratch = tempfile::tempdir().context("failed to create temporary directory for import")?;
    bundle_io::unpack_bundle(
        bundle_path,
        scratch.path(),
        access.password.as_deref(),
        access.is_encrypted,
    )?;

    let manifest = bundle_io::read_manifest_from_dir(scratch.path())?;
    compat::validate_data_compatibility(&manifest, target_version_num)?;
    parallel::import_objects_parallel(
        &target_config,
        scratch.path(),
        &manifest,
        mode,
        concurrency,
        progress_enabled,
    )
    .await?;

    Ok(())
}

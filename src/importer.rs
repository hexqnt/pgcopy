use std::{fmt, path::Path};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;

use crate::bundle_io;
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

impl ImportMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Append => "append",
        }
    }
}

impl fmt::Display for ImportMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Выполняет импорт bundle в target PostgreSQL.
pub async fn run(
    bundle_path: &Path,
    mode: ImportMode,
    concurrency: usize,
    ddl_only: bool,
    bundle_password: Option<&str>,
    target_config: tokio_postgres::Config,
    progress_enabled: bool,
) -> Result<()> {
    if concurrency == 0 {
        bail!("import concurrency must be >= 1");
    }

    let access = bundle_io::resolve_access(bundle_path, bundle_password)?;
    let client = pg::connect(&target_config).await?;
    let target_version_num = if ddl_only {
        0
    } else {
        pg::server_version_num(&client).await?
    };

    // DDL-only всегда выполняем потоково: это избегает дорогой распаковки data/* на диск.
    if ddl_only || concurrency == 1 {
        stream::import_objects_streaming(
            bundle_path,
            &client,
            stream::ImportStreamOptions {
                access,
                mode,
                ddl_only,
                target_version_num,
                progress_enabled,
            },
        )
        .await?;
        return Ok(());
    }

    let scratch = tempfile::tempdir().context("failed to create temporary directory for import")?;
    let unpack_bundle_path = bundle_path.to_path_buf();
    let unpack_scratch_path = scratch.path().to_path_buf();
    let unpack_access = access;
    tokio::task::spawn_blocking(move || {
        bundle_io::unpack_bundle(&unpack_bundle_path, &unpack_scratch_path, &unpack_access)
    })
    .await
    .context("parallel import bundle unpack task failed")??;

    let manifest = bundle_io::read_manifest_from_dir(scratch.path())?;
    compat::validate_data_compatibility(&manifest, target_version_num)?;
    parallel::import_objects_parallel(
        &target_config,
        scratch.path(),
        &manifest,
        mode,
        concurrency,
        ddl_only,
        progress_enabled,
    )
    .await?;

    Ok(())
}

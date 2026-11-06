mod app;
mod bundle_io;
mod cli;
mod config;
mod crypto;
mod describe;
mod env_dsn;
mod export;
mod importer;
mod info;
mod manifest;
mod parallel_workers;
mod pg;
mod pgpass;
mod progress;
mod retry;
mod select_dsl;
mod sql;
mod startup;
mod types;

use anyhow::Result;
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    app::run(cli).await
}

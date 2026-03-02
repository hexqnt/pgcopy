use anyhow::Result;
use clap::Parser;

mod app;
mod bundle_io;
mod cli;
mod config;
mod crypto;
mod env_dsn;
mod export;
mod importer;
mod info;
mod manifest;
mod parallel_workers;
mod pg;
mod progress;
mod select_dsl;
mod sql;
mod startup;
mod types;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    app::run(cli).await
}

use std::{fmt, path::Path};

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::Serialize;

use crate::bundle_io;
use crate::manifest::{Manifest, ManifestObject};
use crate::types::DataFormat;

/// Формат вывода команды `pgcopy info`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum InfoOutputFormat {
    #[value(help = "Human-readable output")]
    Text,
    #[value(help = "Machine-readable JSON output")]
    Json,
}

impl InfoOutputFormat {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
        }
    }
}

impl fmt::Display for InfoOutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Печатает метаинформацию о bundle без подключения к PostgreSQL.
pub fn run(
    bundle_path: &Path,
    bundle_password: Option<&str>,
    output_format: InfoOutputFormat,
    show_objects: bool,
) -> Result<()> {
    let access = bundle_io::resolve_access(bundle_path, bundle_password)?;
    let manifest = bundle_io::read_manifest_from_bundle(bundle_path, &access)?;

    match output_format {
        InfoOutputFormat::Text => {
            print_text(bundle_path, access.is_encrypted, &manifest, show_objects);
        }
        InfoOutputFormat::Json => {
            print_json(bundle_path, access.is_encrypted, &manifest, show_objects)?;
        }
    }

    Ok(())
}

fn print_text(bundle_path: &Path, is_encrypted: bool, manifest: &Manifest, show_objects: bool) {
    println!("bundle: {}", bundle_path.display());
    println!("encrypted: {}", if is_encrypted { "yes" } else { "no" });
    println!("format_version: {}", manifest.format_version);
    println!("created_at: {}", manifest.created_at);
    println!(
        "source_fingerprint: {}",
        manifest.source_fingerprint.as_deref().unwrap_or("-")
    );
    println!("source_pg_version_num: {}", manifest.source_pg_version_num);
    println!("data_format: {}", manifest.data_format);
    println!("consistent_snapshot: {}", manifest.consistent_snapshot);
    println!("objects_count: {}", manifest.objects.len());

    if !show_objects {
        return;
    }

    for (index, object) in manifest.objects.iter().enumerate() {
        println!(
            "{}. {}.{} -> {}.{} ({})",
            index + 1,
            object.source_schema,
            object.source_name,
            object.target_schema,
            object.target_name,
            object.kind
        );
        println!("   projection: {}", object.column_projection);
        println!("   columns: {}", object.effective_columns.join(", "));
        println!(
            "   row_estimate: {}",
            object
                .row_estimate
                .map_or_else(|| "unknown".to_owned(), |value| value.to_string())
        );
    }
}

fn print_json(
    bundle_path: &Path,
    is_encrypted: bool,
    manifest: &Manifest,
    show_objects: bool,
) -> Result<()> {
    // В JSON по умолчанию не включаем массив объектов, чтобы вывод оставался
    // компактным для CLI и стабильным для простых скриптов.
    let output = BundleInfoJson {
        bundle_path: bundle_path.display().to_string(),
        encrypted: is_encrypted,
        format_version: manifest.format_version,
        created_at: &manifest.created_at,
        source_fingerprint: manifest.source_fingerprint.as_deref(),
        source_pg_version_num: manifest.source_pg_version_num,
        data_format: manifest.data_format,
        consistent_snapshot: manifest.consistent_snapshot,
        objects_count: manifest.objects.len(),
        objects: show_objects.then_some(manifest.objects.as_slice()),
    };

    let json = serde_json::to_string_pretty(&output).context("failed to serialize bundle info")?;
    println!("{json}");
    Ok(())
}

#[derive(Serialize)]
struct BundleInfoJson<'a> {
    bundle_path: String,
    encrypted: bool,
    format_version: u32,
    created_at: &'a str,
    source_fingerprint: Option<&'a str>,
    source_pg_version_num: i32,
    data_format: DataFormat,
    consistent_snapshot: bool,
    objects_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    objects: Option<&'a [ManifestObject]>,
}

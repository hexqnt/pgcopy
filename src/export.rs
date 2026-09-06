use std::collections::HashMap;
use std::env;
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use tempfile::TempDir;

use crate::bundle_io;
use crate::config::{self, Config, GeneralConfig, ObjectConfig};
use crate::crypto;
use crate::manifest::{Manifest, ManifestObject};
use crate::parallel_workers;
use crate::pg::{self, RelationKind};
use crate::select_dsl::ProjectionKind;
use crate::types::{DataFormat, ExportAs};

use object::{export_object, validate_target_collisions};
use progress::ExportProgress;
use session::run_with_snapshot_support;
use worker::export_worker;

mod object;
mod progress;
mod session;
mod worker;
type SourceKey = (String, String);

#[derive(Debug)]
struct SeenSource {
    config_index: Option<usize>,
    is_auto_dependency: bool,
    required_by_view_index: Option<usize>,
    dependency_compatible: bool,
    incompatibility_reason: Option<String>,
}

impl SeenSource {
    const fn auto_dependency(required_by_view_index: usize) -> Self {
        Self {
            config_index: None,
            is_auto_dependency: true,
            required_by_view_index: Some(required_by_view_index),
            dependency_compatible: true,
            incompatibility_reason: None,
        }
    }

    fn from_config(config_index: usize, compatibility_issues: &[&'static str]) -> Self {
        let incompatibility_reason =
            (!compatibility_issues.is_empty()).then(|| compatibility_issues.join(", "));
        Self {
            config_index: Some(config_index),
            is_auto_dependency: false,
            required_by_view_index: None,
            dependency_compatible: incompatibility_reason.is_none(),
            incompatibility_reason,
        }
    }

    fn origin_ref(&self) -> String {
        if let Some(config_index) = self.config_index {
            return format!("objects[{config_index}]");
        }
        if let Some(view_index) = self.required_by_view_index {
            return format!("auto-added dependency for objects[{view_index}]");
        }
        "auto-added dependency".to_owned()
    }
}

pub(crate) fn resolve_export_concurrency(
    cli_concurrency: Option<NonZeroUsize>,
    general: &GeneralConfig,
) -> Result<NonZeroUsize> {
    if let Some(concurrency) = cli_concurrency {
        return Ok(concurrency);
    }

    if general.concurrency_from_toml {
        return Ok(general.concurrency);
    }

    // Приоритет параметров: CLI > TOML > ENV > fallback.
    let env_name = "PGCOPY_CONCURRENCY";
    match env::var(env_name) {
        Ok(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(general.concurrency);
            }

            let parsed = trimmed.parse::<NonZeroUsize>().map_err(|_| {
                anyhow::anyhow!("invalid {env_name} value '{trimmed}', expected integer >= 1")
            })?;
            Ok(parsed)
        }
        Err(env::VarError::NotPresent) => Ok(general.concurrency),
        Err(env::VarError::NotUnicode(_)) => {
            bail!("environment variable {env_name} contains non-Unicode data")
        }
    }
}

fn dependency_compatibility_issues(object: &ObjectConfig) -> Vec<&'static str> {
    let mut issues = Vec::new();
    if object.export_as != ExportAs::Table {
        issues.push("export_as must be 'table'");
    }
    if object.target.is_some() {
        issues.push("target_schema/target_name override is not allowed");
    }
    if object.select.projection_kind() != ProjectionKind::All {
        issues.push("select projection must be '*'");
    }
    if object.select.where_clause.is_some()
        || object.select.order_by_clause.is_some()
        || object.select.limit_clause.is_some()
    {
        issues.push("WHERE/ORDER BY/LIMIT are not allowed");
    }
    issues
}

/// Выполняет экспорт объектов в bundle.
pub async fn run(
    config_path: &Path,
    out_path: &Path,
    cli_concurrency: Option<NonZeroUsize>,
    bundle_password: Option<&str>,
    source_config: tokio_postgres::Config,
    progress_enabled: bool,
) -> Result<()> {
    let operation_started_at = Instant::now();
    let config = config::load(config_path)?;
    let concurrency = resolve_export_concurrency(cli_concurrency, &config.general)?;
    let password = crypto::resolve_bundle_password(bundle_password)?;
    let client = pg::connect(&source_config).await?;
    let export_objects_plan = build_export_plan(&client, &config).await?;
    let progress = ExportProgress::new(export_objects_plan.len(), progress_enabled);

    let source_pg_version_num = pg::server_version_num(&client).await?;
    let source_fingerprint = Some(pg::source_fingerprint(&client).await?);

    let scratch = tempfile::tempdir().context("failed to create temporary directory for export")?;
    std::fs::create_dir_all(scratch.path().join("ddl"))?;
    std::fs::create_dir_all(scratch.path().join("data"))?;

    let export_result = if concurrency.get() == 1 {
        run_with_snapshot_support(&client, config.general.consistent_snapshot, false, |_| {
            export_objects(
                &client,
                &export_objects_plan,
                config.general.data_format,
                &scratch,
                &progress,
            )
        })
        .await
    } else {
        run_with_snapshot_support(
            &client,
            config.general.consistent_snapshot,
            true,
            |snapshot_id| {
                export_objects_parallel(
                    &source_config,
                    &export_objects_plan,
                    config.general.data_format,
                    &scratch,
                    &progress,
                    concurrency.get(),
                    snapshot_id,
                )
            },
        )
        .await
    };

    let manifest_objects = match export_result {
        Ok(manifest_objects) => manifest_objects,
        Err(error) => {
            progress.finish_with_error(error.as_ref());
            return Err(error);
        }
    };

    let manifest = Manifest {
        format_version: 2,
        created_at: Utc::now().to_rfc3339(),
        source_fingerprint,
        source_pg_version_num,
        data_format: config.general.data_format,
        consistent_snapshot: config.general.consistent_snapshot,
        objects: manifest_objects,
    };

    progress.set_bundle_running();
    let bundle_scratch_path = scratch.path().to_path_buf();
    let bundle_out_path = out_path.to_path_buf();
    let bundle_password = password;
    let bundle_manifest = manifest;
    let write_result = tokio::task::spawn_blocking(move || {
        bundle_io::write_bundle(
            &bundle_scratch_path,
            &bundle_out_path,
            &bundle_manifest,
            bundle_password.as_ref(),
        )
    })
    .await
    .context("bundle writer task failed")?;
    match write_result {
        Ok(()) => {
            progress.finish_bundle_done(out_path, operation_started_at.elapsed());
            Ok(())
        }
        Err(error) => {
            progress.finish_bundle_error(out_path, error.as_ref());
            Err(error)
        }
    }
}

async fn export_objects(
    client: &tokio_postgres::Client,
    objects: &[ObjectConfig],
    data_format: DataFormat,
    scratch: &TempDir,
    progress: &ExportProgress,
) -> Result<Vec<ManifestObject>> {
    let mut manifest_objects = Vec::with_capacity(objects.len());

    for (index, object) in objects.iter().enumerate() {
        progress.set_object_running(object);
        let object_started_at = Instant::now();
        let manifest_object = export_object(client, scratch.path(), index, object, data_format)
            .await
            .with_context(|| format!("export object {} failed", object.source_label()));

        match manifest_object {
            Ok(manifest_object) => {
                progress.set_object_done(&manifest_object, object_started_at.elapsed());
                manifest_objects.push(manifest_object);
            }
            Err(error) => {
                progress.set_object_error(object, error.as_ref());
                return Err(error);
            }
        }
    }

    validate_target_collisions(&manifest_objects)?;
    Ok(manifest_objects)
}

async fn export_objects_parallel(
    source_config: &tokio_postgres::Config,
    objects: &[ObjectConfig],
    data_format: DataFormat,
    scratch: &TempDir,
    progress: &ExportProgress,
    concurrency: usize,
    snapshot_id: Option<String>,
) -> Result<Vec<ManifestObject>> {
    let mut ordered_objects = vec![None; objects.len()];
    for object in objects {
        progress.set_object_running(object);
    }
    let mut workers = parallel_workers::spawn_bucket_workers(objects, concurrency, |tasks| {
        let source_config = source_config.clone();
        let scratch_dir = scratch.path().to_path_buf();
        let snapshot_id = snapshot_id.clone();
        // Каждый worker получает свой connection и обрабатывает свой bucket.
        async move {
            export_worker(
                &source_config,
                &scratch_dir,
                tasks,
                data_format,
                snapshot_id.as_deref(),
            )
            .await
        }
    });

    parallel_workers::process_joinset_outcomes(
        &mut workers,
        "parallel export worker task failed",
        |outcome| {
            for (index, result) in outcome.completed {
                progress.set_object_done(&result.manifest_object, result.elapsed);
                ordered_objects[index] = Some(result.manifest_object);
            }

            if let Some(failure) = outcome.failure {
                progress.set_object_error(&failure.task, failure.error.as_ref());
                return Err(failure.error);
            }

            Ok(())
        },
    )
    .await?;

    let mut manifest_objects = Vec::with_capacity(objects.len());
    for (index, manifest_object) in ordered_objects.into_iter().enumerate() {
        let manifest_object = manifest_object.with_context(|| {
            format!(
                "internal error: missing export result for object index {}",
                index + 1
            )
        })?;
        manifest_objects.push(manifest_object);
    }

    validate_target_collisions(&manifest_objects)?;
    Ok(manifest_objects)
}

async fn build_export_plan(
    client: &tokio_postgres::Client,
    config: &Config,
) -> Result<Vec<ObjectConfig>> {
    let mut planned = Vec::with_capacity(config.objects.len());
    let mut seen_sources: HashMap<SourceKey, SeenSource> = HashMap::new();

    for (index, object) in config.objects.iter().enumerate() {
        if object.export_as == ExportAs::View {
            validate_view_source_kind(client, object, index).await?;
            let dependencies = pg::view_dependencies_transitive(
                client,
                object.source_schema(),
                object.source_name(),
            )
            .await?;

            for dependency in dependencies {
                let source_key = (dependency.schema.clone(), dependency.name.clone());
                if let Some(existing) = seen_sources.get(&source_key) {
                    if existing.dependency_compatible {
                        continue;
                    }

                    let existing_ref = existing.origin_ref();
                    let reason = existing
                        .incompatibility_reason
                        .as_deref()
                        .unwrap_or("dependency requirements are not satisfied");
                    bail!(
                        "config validation error: source relation {}.{} is required as a dependency for objects[{index}] export_as='view', but {existing_ref} is incompatible ({reason}); dependency object must be exported as full table with default target using 'select * from schema.object' without WHERE/ORDER BY/LIMIT",
                        dependency.schema,
                        dependency.name
                    );
                }

                let dependency_object =
                    ObjectConfig::from_dependency(&dependency.schema, &dependency.name)?;
                seen_sources.insert(source_key, SeenSource::auto_dependency(index));
                planned.push(dependency_object);
            }
        }

        let source_key = (
            object.source_schema().to_owned(),
            object.source_name().to_owned(),
        );

        let compatibility_issues = dependency_compatibility_issues(object);
        if let Some(existing) = seen_sources.get_mut(&source_key) {
            if existing.is_auto_dependency
                && object.export_as == ExportAs::View
                && object.target.is_none()
            {
                let required_by_ref = existing.required_by_view_index.map_or_else(
                    || "another export_as='view' object".to_owned(),
                    |view_index| format!("objects[{view_index}]"),
                );
                bail!(
                    "config validation error: objects[{index}] export_as='view' for source {}.{} conflicts with auto-added dependency table for the same source (required by {required_by_ref}); set target_schema/target_name for one of these objects to avoid target name collision",
                    object.source_schema(),
                    object.source_name()
                );
            }

            if existing.is_auto_dependency
                && object.export_as == ExportAs::Table
                && object.target.is_none()
            {
                if compatibility_issues.is_empty() {
                    continue;
                }

                let required_by_ref = existing.required_by_view_index.map_or_else(
                    || "another export_as='view' object".to_owned(),
                    |view_index| format!("objects[{view_index}]"),
                );
                bail!(
                    "config validation error: objects[{index}] for source {}.{} conflicts with auto-added dependency table (required by {required_by_ref}): {}; to use default target names, this object must satisfy dependency rules",
                    object.source_schema(),
                    object.source_name(),
                    compatibility_issues.join(", ")
                );
            }

            if compatibility_issues.is_empty() {
                existing.dependency_compatible = true;
                existing.incompatibility_reason = None;
            } else if existing.incompatibility_reason.is_none() {
                existing.incompatibility_reason = Some(compatibility_issues.join(", "));
            }

            planned.push(object.clone());
            continue;
        }

        seen_sources.insert(
            source_key,
            SeenSource::from_config(index, &compatibility_issues),
        );
        planned.push(object.clone());
    }

    Ok(planned)
}

async fn validate_view_source_kind(
    client: &tokio_postgres::Client,
    object: &ObjectConfig,
    index: usize,
) -> Result<()> {
    let source_kind =
        pg::relation_kind(client, object.source_schema(), object.source_name()).await?;
    if source_kind != RelationKind::View {
        bail!(
            "config validation error: objects[{index}] export_as='view' requires source relation {}.{} to be a view, got {}",
            object.source_schema(),
            object.source_name(),
            source_kind.as_str()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::dependency_compatibility_issues;
    use crate::config::{ObjectConfig, TargetOverride};
    use crate::select_dsl::SelectDsl;
    use crate::sql::Identifier;
    use crate::types::ExportAs;

    fn object_config(select_raw: &str, export_as: ExportAs) -> ObjectConfig {
        ObjectConfig {
            select_raw: select_raw.to_owned(),
            select: SelectDsl::parse(select_raw).expect("select must parse in tests"),
            target: None,
            export_as,
        }
    }

    #[test]
    fn dependency_compatibility_accepts_full_table_default() {
        let object = object_config("select * from public.orders", ExportAs::Table);
        let issues = dependency_compatibility_issues(&object);
        assert_eq!(issues, [] as [&str; 0]);
    }

    #[test]
    fn dependency_compatibility_rejects_filtered_projection_and_target_override() {
        let mut object = object_config(
            "select id from public.orders where id > 10",
            ExportAs::Table,
        );
        object.target = Some(TargetOverride {
            schema: Identifier::parse("archive", "target_schema").expect("valid schema"),
            name: Identifier::parse("orders", "target_name").expect("valid target"),
        });

        let issues = dependency_compatibility_issues(&object);
        assert!(issues.contains(&"target_schema/target_name override is not allowed"));
        assert!(issues.contains(&"select projection must be '*'"));
        assert!(issues.contains(&"WHERE/ORDER BY/LIMIT are not allowed"));
    }
}

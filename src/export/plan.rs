use std::collections::HashMap;

use anyhow::{Result, bail};

use crate::config::ObjectConfig;
use crate::pg::{self, RelationKind};
use crate::select_dsl::ProjectionKind;
use crate::types::ExportAs;

type SourceKey = (String, String);

#[derive(Debug, Clone, Copy)]
enum SourceOrigin {
    Config(usize),
    Dependency(usize),
}

#[derive(Debug)]
struct SeenSource {
    origin: SourceOrigin,
    incompatibility_reason: Option<String>,
}

impl SeenSource {
    const fn auto_dependency(required_by_view_index: usize) -> Self {
        Self {
            origin: SourceOrigin::Dependency(required_by_view_index),
            incompatibility_reason: None,
        }
    }

    fn from_config(config_index: usize, compatibility_issues: &[&'static str]) -> Self {
        let incompatibility_reason =
            (!compatibility_issues.is_empty()).then(|| compatibility_issues.join(", "));
        Self {
            origin: SourceOrigin::Config(config_index),
            incompatibility_reason,
        }
    }

    fn origin_ref(&self) -> String {
        match self.origin {
            SourceOrigin::Config(index) => format!("objects[{index}]"),
            SourceOrigin::Dependency(index) => {
                format!("auto-added dependency for objects[{index}]")
            }
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
    if object.select.has_clauses() {
        issues.push("WHERE/ORDER BY/LIMIT are not allowed");
    }
    issues
}

pub(super) async fn build_export_plan(
    client: &tokio_postgres::Client,
    objects: Vec<ObjectConfig>,
) -> Result<Vec<ObjectConfig>> {
    let mut planned = Vec::with_capacity(objects.len());
    let mut seen_sources: HashMap<SourceKey, SeenSource> = HashMap::new();

    for (index, object) in objects.into_iter().enumerate() {
        if object.export_as == ExportAs::View {
            validate_view_source_kind(client, &object, index).await?;
            let dependencies = pg::view_dependencies_transitive(
                client,
                object.source_schema(),
                object.source_name(),
            )
            .await?;

            for dependency in dependencies {
                let source_key = (dependency.schema.clone(), dependency.name.clone());
                if let Some(existing) = seen_sources.get(&source_key) {
                    let Some(reason) = existing.incompatibility_reason.as_deref() else {
                        continue;
                    };
                    let existing_ref = existing.origin_ref();
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

        let compatibility_issues = dependency_compatibility_issues(&object);
        if let Some(existing) = seen_sources.get_mut(&source_key) {
            if let SourceOrigin::Dependency(view_index) = existing.origin
                && object.target.is_none()
            {
                if object.export_as == ExportAs::View {
                    bail!(
                        "config validation error: objects[{index}] export_as='view' for source {}.{} conflicts with auto-added dependency table for the same source (required by objects[{view_index}]); set target_schema/target_name for one of these objects to avoid target name collision",
                        object.source_schema(),
                        object.source_name()
                    );
                }
                if compatibility_issues.is_empty() {
                    continue;
                }
                bail!(
                    "config validation error: objects[{index}] for source {}.{} conflicts with auto-added dependency table (required by objects[{view_index}]): {}; to use default target names, this object must satisfy dependency rules",
                    object.source_schema(),
                    object.source_name(),
                    compatibility_issues.join(", ")
                );
            }

            if compatibility_issues.is_empty() {
                existing.incompatibility_reason = None;
            }

            planned.push(object);
            continue;
        }

        seen_sources.insert(
            source_key,
            SeenSource::from_config(index, &compatibility_issues),
        );
        planned.push(object);
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

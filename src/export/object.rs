use std::path::Path;

use anyhow::{Context, Result, bail};
use futures_util::{TryStreamExt, pin_mut};
use tokio::io::AsyncWriteExt;

use crate::config::ObjectConfig;
use crate::manifest::ManifestObject;
use crate::pg::{self, RelationKind};
use crate::types::DataFormat;

/// Экспортирует один объект в scratch-структуру и формирует запись manifest.
pub async fn export_object(
    client: &tokio_postgres::Client,
    scratch_dir: &Path,
    index: usize,
    object: &ObjectConfig,
    data_format: DataFormat,
) -> Result<ManifestObject> {
    let source_schema = object.select.source_schema.clone();
    let source_name = object.select.source_name.clone();

    let relation_kind = pg::relation_kind(client, &source_schema, &source_name).await?;
    let (target_schema, target_name) = resolve_target_names(object, relation_kind);

    let all_defs = pg::relation_column_defs(client, &source_schema, &source_name).await?;
    let source_columns = all_defs
        .iter()
        .map(|definition| definition.name.clone())
        .collect::<Vec<_>>();
    let effective_columns = object
        .select
        .effective_columns(&source_columns)
        .with_context(|| {
            format!("failed to build effective columns for {source_schema}.{source_name}")
        })?;
    let selected_defs =
        pg::pick_column_defs(&all_defs, &effective_columns, &source_schema, &source_name)?;
    let effective_column_types = selected_defs
        .iter()
        .map(|definition| definition.type_sql.clone())
        .collect::<Vec<_>>();

    let normalized_select = object.select.normalized_select_sql(&effective_columns);
    let copy_out_sql = pg::copy_out_sql(&normalized_select, data_format);
    let ddl_sql = pg::create_table_ddl(&target_schema, &target_name, &selected_defs);

    let stem = format!("{:04}__{source_schema}.{source_name}", index + 1);
    let ddl_path = format!("ddl/{stem}.sql");
    let data_path = format!("data/{stem}.{}", data_file_suffix(data_format));

    let ddl_file = scratch_dir.join(&ddl_path);
    let data_file = scratch_dir.join(&data_path);

    if let Some(parent) = ddl_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(parent) = data_file.parent() {
        std::fs::create_dir_all(parent)?;
    }

    tokio::fs::write(&ddl_file, ddl_sql)
        .await
        .with_context(|| format!("failed to write DDL file {}", ddl_file.display()))?;

    let mut output = tokio::fs::File::create(&data_file)
        .await
        .with_context(|| format!("failed to create data file {}", data_file.display()))?;

    let copy_stream = client
        .copy_out(&copy_out_sql)
        .await
        .with_context(|| format!("failed to execute COPY OUT for {source_schema}.{source_name}"))?;
    pin_mut!(copy_stream);

    while let Some(chunk) = copy_stream.as_mut().try_next().await? {
        output
            .write_all(&chunk)
            .await
            .with_context(|| format!("failed to write data chunk to {}", data_file.display()))?;
    }
    output.flush().await?;

    let row_estimate = pg::row_estimate(client, &source_schema, &source_name).await?;

    Ok(ManifestObject {
        kind: relation_kind.as_str().to_owned(),
        source_schema,
        source_name,
        target_schema,
        target_name,
        source_select: object.select_raw.clone(),
        normalized_select,
        ddl_path,
        data_path,
        effective_columns,
        effective_column_types,
        column_projection: object.select.projection_kind().to_owned(),
        row_estimate,
    })
}

/// Проверяет, что в manifest нет коллизий целевых имён.
pub fn validate_target_collisions(objects: &[ManifestObject]) -> Result<()> {
    use std::collections::HashSet;

    let mut seen = HashSet::new();
    for object in objects {
        let key = format!("{}.{}", object.target_schema, object.target_name);
        if !seen.insert(key.clone()) {
            bail!("target name collision detected for {key}");
        }
    }

    Ok(())
}

fn data_file_suffix(data_format: DataFormat) -> &'static str {
    match data_format {
        DataFormat::Binary => "copybin",
        DataFormat::Csv => "copycsv",
    }
}

fn resolve_target_names(object: &ObjectConfig, relation_kind: RelationKind) -> (String, String) {
    match relation_kind {
        RelationKind::Table | RelationKind::View | RelationKind::MaterializedView => (
            object
                .target_schema
                .clone()
                .unwrap_or_else(|| object.select.source_schema.clone()),
            object
                .target_name
                .clone()
                .unwrap_or_else(|| object.select.source_name.clone()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_target_names;
    use crate::config::ObjectConfig;
    use crate::pg::RelationKind;
    use crate::select_dsl::SelectDsl;

    fn object_without_target(select: &str) -> ObjectConfig {
        ObjectConfig {
            select_raw: select.to_owned(),
            select: SelectDsl::parse(select).expect("select should be valid"),
            target_schema: None,
            target_name: None,
        }
    }

    #[test]
    fn materialized_view_uses_default_target_names() {
        let object = object_without_target("select * from reporting.sales_daily_mv");
        let names = resolve_target_names(&object, RelationKind::MaterializedView);
        assert_eq!(names, ("reporting".to_owned(), "sales_daily_mv".to_owned()));
    }

    #[test]
    fn plain_view_uses_default_target_names() {
        let object = object_without_target("select * from reporting.sales_daily_view");
        let names = resolve_target_names(&object, RelationKind::View);
        assert_eq!(
            names,
            ("reporting".to_owned(), "sales_daily_view".to_owned())
        );
    }
}

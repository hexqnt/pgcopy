use std::path::Path;

use anyhow::{Context, Result, bail};
use async_compression::{Level, tokio::write::ZstdEncoder};
use futures_util::{TryStreamExt, pin_mut};
use tokio::io::{AsyncWrite, AsyncWriteExt, BufWriter};

use crate::config::ObjectConfig;
use crate::manifest::ManifestObject;
use crate::pg::{self, ColumnDef, RelationKind};
use crate::scratch::{ScratchData, TempCompression};
use crate::types::{DataFormat, ExportAs};

// Укрупняет запись COPY OUT в файл и не создаёт blocking-задачу Tokio
// для каждого небольшого чанка протокола PostgreSQL.
const COPY_FILE_BUFFER_SIZE: usize = 1024 * 1024;

pub(super) struct ExportedObject {
    pub manifest: ManifestObject,
    pub data: ScratchData,
}

/// Проверяет, что в manifest нет коллизий целевых имён.
pub fn validate_target_collisions(objects: &[ManifestObject]) -> Result<()> {
    use std::collections::HashSet;

    let mut seen = HashSet::new();
    for object in objects {
        let key = (object.target_schema.as_str(), object.target_name.as_str());
        if !seen.insert(key) {
            bail!(
                "target name collision detected for {}.{}",
                object.target_schema,
                object.target_name
            );
        }
    }

    Ok(())
}

const fn data_file_suffix(data_format: DataFormat) -> &'static str {
    match data_format {
        DataFormat::Binary => "copybin",
        DataFormat::Csv => "copycsv",
    }
}

fn resolve_target_names(object: &ObjectConfig) -> (String, String) {
    if let Some(target) = object.target.as_ref() {
        return (target.schema.to_string(), target.name.to_string());
    }

    (
        object.select.source_schema.clone(),
        object.select.source_name.clone(),
    )
}

/// Экспортирует один объект в scratch-структуру и формирует запись manifest.
pub async fn export_object(
    client: &tokio_postgres::Client,
    scratch_dir: &Path,
    index: usize,
    object: &ObjectConfig,
    data_format: DataFormat,
    temp_compression: TempCompression,
) -> Result<ExportedObject> {
    let source_schema = object.source_schema().to_owned();
    let source_name = object.source_name().to_owned();

    let relation_kind = pg::relation_kind(client, &source_schema, &source_name).await?;
    let (target_schema, target_name) = resolve_target_names(object);
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

    if object.export_as == ExportAs::View && relation_kind != RelationKind::View {
        bail!(
            "export_as='view' requires source relation {}.{} to be a view, got {}",
            source_schema,
            source_name,
            relation_kind.as_str()
        );
    }
    let all_defs = match object.export_as {
        ExportAs::Table => pg::relation_column_defs(client, &source_schema, &source_name).await?,
        ExportAs::View => pg::relation_columns_with_types(client, &source_schema, &source_name)
            .await?
            .into_iter()
            .map(|(name, type_sql)| ColumnDef {
                name,
                type_sql,
                not_null: false,
                default_expr: None,
            })
            .collect(),
    };
    let source_columns = all_defs
        .iter()
        .map(|column| column.name.as_str())
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
        .map(|column| column.type_sql.clone())
        .collect();
    let normalized_select = object.select.normalized_select_sql(&effective_columns);
    let ddl_sql = match object.export_as {
        ExportAs::Table => {
            pg::create_table_ddl(&target_schema, &target_name, selected_defs.iter().copied())
        }
        ExportAs::View => {
            let view_sql = pg::view_definition_sql(client, &source_schema, &source_name).await?;
            pg::create_view_ddl(&target_schema, &target_name, &view_sql)
        }
    };
    tokio::fs::write(&ddl_file, ddl_sql)
        .await
        .with_context(|| format!("failed to write DDL file {}", ddl_file.display()))?;

    let (row_estimate, data) = if object.export_as.requires_data_payload() {
        let copy_out_sql = pg::copy_out_sql(&normalized_select, data_format);
        let data = write_copy_data(
            client,
            &copy_out_sql,
            &data_file,
            &source_schema,
            &source_name,
            temp_compression,
        )
        .await?;
        (
            pg::row_estimate(client, &source_schema, &source_name).await?,
            data,
        )
    } else {
        tokio::fs::File::create(&data_file)
            .await
            .with_context(|| format!("failed to create data file {}", data_file.display()))?;
        (None, ScratchData::Plain)
    };

    let manifest = ManifestObject {
        kind: relation_kind,
        export_as: object.export_as,
        source_schema,
        source_name,
        target_schema,
        target_name,
        source_select: object.select.raw.clone(),
        normalized_select,
        ddl_path,
        data_path,
        effective_columns,
        effective_column_types,
        column_projection: object.select.projection_kind(),
        row_estimate,
    };
    Ok(ExportedObject { manifest, data })
}

async fn write_copy_data(
    client: &tokio_postgres::Client,
    copy_out_sql: &str,
    data_file: &Path,
    source_schema: &str,
    source_name: &str,
    compression: TempCompression,
) -> Result<ScratchData> {
    let data_file = compression.path(data_file);
    let output = tokio::fs::File::create(&data_file)
        .await
        .with_context(|| format!("failed to create data file {}", data_file.display()))?;
    let output = BufWriter::with_capacity(COPY_FILE_BUFFER_SIZE, output);

    match compression {
        TempCompression::None => {
            write_copy_stream(client, copy_out_sql, output, source_schema, source_name).await?;
            Ok(ScratchData::Plain)
        }
        TempCompression::Zstd => {
            let output = ZstdEncoder::with_quality(output, Level::Precise(3));
            let uncompressed_size =
                write_copy_stream(client, copy_out_sql, output, source_schema, source_name).await?;
            Ok(ScratchData::Zstd { uncompressed_size })
        }
    }
}

async fn write_copy_stream<W: AsyncWrite + Unpin>(
    client: &tokio_postgres::Client,
    copy_out_sql: &str,
    mut output: W,
    source_schema: &str,
    source_name: &str,
) -> Result<u64> {
    let copy_stream = client
        .copy_out(copy_out_sql)
        .await
        .with_context(|| format!("failed to execute COPY OUT for {source_schema}.{source_name}"))?;
    pin_mut!(copy_stream);
    let mut size = 0;

    while let Some(chunk) = copy_stream
        .as_mut()
        .try_next()
        .await
        .with_context(|| format!("failed to read COPY OUT for {source_schema}.{source_name}"))?
    {
        output.write_all(&chunk).await.with_context(|| {
            format!("failed to write COPY data for {source_schema}.{source_name}")
        })?;
        size += chunk.len() as u64;
    }

    output.shutdown().await.with_context(|| {
        format!("failed to finalize COPY data for {source_schema}.{source_name}")
    })?;
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::resolve_target_names;
    use crate::config::{ObjectConfig, TargetOverride};
    use crate::select_dsl::SelectDsl;
    use crate::sql::Identifier;
    use crate::types::ExportAs;

    fn object_without_target(select: &str) -> ObjectConfig {
        ObjectConfig {
            select: SelectDsl::parse(select).expect("select should be valid"),
            target: None,
            export_as: ExportAs::Table,
        }
    }

    #[test]
    fn default_target_names_follow_source() {
        let object = object_without_target("select * from reporting.sales_daily_mv");
        let names = resolve_target_names(&object);
        assert_eq!(names, ("reporting".to_owned(), "sales_daily_mv".to_owned()));
    }

    #[test]
    fn explicit_target_names_override_source() {
        let mut object = object_without_target("select * from reporting.sales_daily_view");
        object.target = Some(TargetOverride {
            schema: Identifier::parse("archive", "target_schema").expect("valid schema"),
            name: Identifier::parse("sales_copy", "target_name").expect("valid name"),
        });
        let names = resolve_target_names(&object);
        assert_eq!(names, ("archive".to_owned(), "sales_copy".to_owned()));
    }
}

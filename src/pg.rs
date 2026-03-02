use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use tokio_postgres::{Client, Config, NoTls};

use crate::sql::{quote_ident, quoted_fq_name};
use crate::types::DataFormat;

/// Тип PostgreSQL-реляции, поддерживаемый инструментом.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKind {
    Table,
    View,
    MaterializedView,
}

impl RelationKind {
    /// Стабильное строковое представление для manifest и сообщений.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::View => "view",
            Self::MaterializedView => "materialized_view",
        }
    }
}

/// Минимальное описание колонки для генерации целевого DDL.
#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: String,
    pub type_sql: String,
    pub not_null: bool,
    pub default_expr: Option<String>,
}

/// Устанавливает подключение к PostgreSQL и запускает background-task драйвера.
pub async fn connect(config: &Config) -> Result<Client> {
    let (client, connection) = config
        .connect(NoTls)
        .await
        .context("failed to connect to PostgreSQL")?;

    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::error!(%error, "postgres connection task failed");
        }
    });

    Ok(client)
}

/// Возвращает `SHOW server_version_num` как целое число.
pub async fn server_version_num(client: &Client) -> Result<i32> {
    let row = client
        .query_one("SHOW server_version_num", &[])
        .await
        .context("failed to fetch server_version_num")?;

    let raw: String = row.get(0);
    let parsed = raw
        .parse::<i32>()
        .with_context(|| format!("invalid server_version_num '{raw}'"))?;

    Ok(parsed)
}

/// Короткий fingerprint источника для manifest.
pub async fn source_fingerprint(client: &Client) -> Result<String> {
    let row = client
        .query_one("SELECT current_database(), current_user", &[])
        .await
        .context("failed to fetch source fingerprint")?;

    let database: String = row.get(0);
    let user: String = row.get(1);
    Ok(format!("database={database} user={user}"))
}

/// Возвращает тип существующей реляции или ошибку, если объект не найден.
pub async fn relation_kind(client: &Client, schema: &str, name: &str) -> Result<RelationKind> {
    relation_kind_opt(client, schema, name)
        .await?
        .with_context(|| format!("source relation {schema}.{name} does not exist"))
}

/// Возвращает тип реляции или `None`, если объект не существует.
pub async fn relation_kind_opt(
    client: &Client,
    schema: &str,
    name: &str,
) -> Result<Option<RelationKind>> {
    let row = client
        .query_opt(
            "
            SELECT c.relkind::text
            FROM pg_class c
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = $1 AND c.relname = $2
            ",
            &[&schema, &name],
        )
        .await
        .with_context(|| format!("failed to resolve relation kind for {schema}.{name}"))?;

    row.map(|row| row.get::<_, String>(0))
        .map(|relkind| relation_kind_from_relkind(&relkind, schema, name))
        .transpose()
}

/// Читает список `(имя, type_sql)` в физическом порядке.
pub async fn relation_columns_with_types(
    client: &Client,
    schema: &str,
    name: &str,
) -> Result<Vec<(String, String)>> {
    let rows = client
        .query(
            "
            SELECT
                a.attname,
                pg_catalog.format_type(a.atttypid, a.atttypmod) AS type_sql
            FROM pg_attribute a
            JOIN pg_class c ON c.oid = a.attrelid
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = $1
              AND c.relname = $2
              AND a.attnum > 0
              AND NOT a.attisdropped
            ORDER BY a.attnum
            ",
            &[&schema, &name],
        )
        .await
        .with_context(|| format!("failed to fetch typed column list for {schema}.{name}"))?;

    if rows.is_empty() {
        bail!("source relation {schema}.{name} has no columns");
    }

    Ok(rows
        .into_iter()
        .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
        .collect())
}

/// Читает определения колонок, включая default/not null.
pub async fn relation_column_defs(
    client: &Client,
    schema: &str,
    name: &str,
) -> Result<Vec<ColumnDef>> {
    let rows = client
        .query(
            "
            SELECT
                a.attname,
                pg_catalog.format_type(a.atttypid, a.atttypmod) AS type_sql,
                a.attnotnull,
                pg_catalog.pg_get_expr(ad.adbin, ad.adrelid) AS default_expr
            FROM pg_attribute a
            JOIN pg_class c ON c.oid = a.attrelid
            JOIN pg_namespace n ON n.oid = c.relnamespace
            LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
            WHERE n.nspname = $1
              AND c.relname = $2
              AND a.attnum > 0
              AND NOT a.attisdropped
            ORDER BY a.attnum
            ",
            &[&schema, &name],
        )
        .await
        .with_context(|| format!("failed to fetch column definitions for {schema}.{name}"))?;

    if rows.is_empty() {
        bail!("source relation {schema}.{name} has no columns");
    }

    Ok(rows
        .into_iter()
        .map(|row| ColumnDef {
            name: row.get(0),
            type_sql: row.get(1),
            not_null: row.get(2),
            default_expr: row.get(3),
        })
        .collect())
}

/// Возвращает приблизительную оценку числа строк из `pg_class.reltuples`.
pub async fn row_estimate(client: &Client, schema: &str, name: &str) -> Result<Option<i64>> {
    let row = client
        .query_opt(
            "
            SELECT c.reltuples::bigint
            FROM pg_class c
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = $1
              AND c.relname = $2
            ",
            &[&schema, &name],
        )
        .await
        .with_context(|| format!("failed to fetch row estimate for {schema}.{name}"))?;

    Ok(row.map(|row| row.get(0)))
}

/// Выбирает определения только для эффективного списка колонок.
pub fn pick_column_defs(
    all_defs: &[ColumnDef],
    effective_columns: &[String],
    source_schema: &str,
    source_name: &str,
) -> Result<Vec<ColumnDef>> {
    let by_name = all_defs
        .iter()
        .map(|definition| (definition.name.as_str(), definition))
        .collect::<HashMap<_, _>>();

    effective_columns
        .iter()
        .map(|column| {
            by_name
                .get(column.as_str())
                .map(|definition| (*definition).clone())
                .with_context(|| {
                    format!(
                        "column definition for {source_schema}.{source_name}.{column} is missing"
                    )
                })
        })
        .collect()
}

/// Генерирует DDL целевой таблицы для импорта.
///
/// Отдельно создаёт локальные sequence для serial-like default выражений,
/// чтобы не ссылаться на sequence исходной БД.
pub fn create_table_ddl(target_schema: &str, target_name: &str, columns: &[ColumnDef]) -> String {
    let mut sequence_specs = Vec::new();
    let body = columns
        .iter()
        .map(|column| {
            let mut parts = vec![format!("{} {}", quote_ident(&column.name), column.type_sql)];

            if let Some(default_expr) = column.default_expr.as_deref() {
                if is_regclass_nextval_default(default_expr) {
                    // Явно пере-привязываем serial/identity-подобный default
                    // к sequence в целевой схеме.
                    let sequence_name = sequence_name_for_column(target_name, &column.name);
                    let sequence_fq_name = quoted_fq_name(target_schema, &sequence_name);
                    sequence_specs.push((sequence_name, column.name.clone()));
                    parts.push(format!("DEFAULT nextval('{sequence_fq_name}'::regclass)"));
                } else {
                    parts.push(format!("DEFAULT {default_expr}"));
                }
            }

            if column.not_null {
                parts.push("NOT NULL".to_owned());
            }

            parts.join(" ")
        })
        .collect::<Vec<_>>()
        .join(",\n    ");

    let mut ddl = String::new();
    for (sequence_name, _) in &sequence_specs {
        ddl.push_str(&format!(
            "CREATE SEQUENCE {};\n",
            quoted_fq_name(target_schema, sequence_name)
        ));
    }

    ddl.push_str(&format!(
        "CREATE TABLE {} (\n    {body}\n);\n",
        quoted_fq_name(target_schema, target_name)
    ));

    for (sequence_name, column_name) in &sequence_specs {
        ddl.push_str(&format!(
            "ALTER SEQUENCE {} OWNED BY {}.{};\n",
            quoted_fq_name(target_schema, sequence_name),
            quoted_fq_name(target_schema, target_name),
            quote_ident(column_name)
        ));
    }

    ddl
}

/// Формирует SQL для `COPY ... TO STDOUT`.
pub fn copy_out_sql(normalized_select_sql: &str, data_format: DataFormat) -> String {
    format!(
        "COPY ({normalized_select_sql}) TO STDOUT ({})",
        copy_format_options(data_format)
    )
}

/// Формирует SQL для `COPY ... FROM STDIN`.
pub fn copy_in_sql(
    target_schema: &str,
    target_name: &str,
    effective_columns: &[String],
    data_format: DataFormat,
) -> String {
    let quoted_columns = effective_columns
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        "COPY {} ({quoted_columns}) FROM STDIN ({})",
        quoted_fq_name(target_schema, target_name),
        copy_format_options(data_format)
    )
}

fn copy_format_options(data_format: DataFormat) -> &'static str {
    match data_format {
        DataFormat::Binary => "FORMAT binary",
        // Keep NULL marker explicit and symmetric for export/import.
        DataFormat::Csv => "FORMAT csv, NULL '\\\\N'",
    }
}

fn is_regclass_nextval_default(default_expr: &str) -> bool {
    let expr = default_expr.trim();
    expr.starts_with("nextval(") && expr.ends_with("::regclass)")
}

fn sequence_name_for_column(target_name: &str, column_name: &str) -> String {
    format!("{target_name}_{column_name}_seq")
}

fn relation_kind_from_relkind(relkind: &str, schema: &str, name: &str) -> Result<RelationKind> {
    match relkind {
        "r" | "p" | "f" => Ok(RelationKind::Table),
        "v" => Ok(RelationKind::View),
        "m" => Ok(RelationKind::MaterializedView),
        other => bail!("unsupported relation kind '{other}' for {schema}.{name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ColumnDef, RelationKind, copy_in_sql, copy_out_sql, create_table_ddl,
        relation_kind_from_relkind,
    };
    use crate::types::DataFormat;

    #[test]
    fn builds_binary_copy_sql() {
        let out = copy_out_sql("SELECT id FROM public.orders", DataFormat::Binary);
        assert_eq!(
            out,
            "COPY (SELECT id FROM public.orders) TO STDOUT (FORMAT binary)"
        );

        let input = copy_in_sql("public", "orders", &["id".to_owned()], DataFormat::Binary);
        assert_eq!(
            input,
            "COPY \"public\".\"orders\" (\"id\") FROM STDIN (FORMAT binary)"
        );
    }

    #[test]
    fn builds_csv_copy_sql() {
        let out = copy_out_sql("SELECT id FROM public.orders", DataFormat::Csv);
        assert_eq!(
            out,
            "COPY (SELECT id FROM public.orders) TO STDOUT (FORMAT csv, NULL '\\\\N')"
        );

        let input = copy_in_sql("public", "orders", &["id".to_owned()], DataFormat::Csv);
        assert_eq!(
            input,
            "COPY \"public\".\"orders\" (\"id\") FROM STDIN (FORMAT csv, NULL '\\\\N')"
        );
    }

    #[test]
    fn maps_materialized_view_relkind() {
        let kind = relation_kind_from_relkind("m", "reporting", "sales_mv")
            .expect("materialized view relkind should be supported");
        assert_eq!(kind, RelationKind::MaterializedView);
        assert_eq!(kind.as_str(), "materialized_view");
    }

    #[test]
    fn rewrites_serial_default_to_target_local_sequence() {
        let ddl = create_table_ddl(
            "archive",
            "orders",
            &[ColumnDef {
                name: "id".to_owned(),
                type_sql: "bigint".to_owned(),
                not_null: true,
                default_expr: Some("nextval('public.orders_id_seq'::regclass)".to_owned()),
            }],
        );

        assert!(ddl.contains("CREATE SEQUENCE \"archive\".\"orders_id_seq\";"));
        assert!(
            ddl.contains("DEFAULT nextval('\"archive\".\"orders_id_seq\"'::regclass) NOT NULL")
        );
        assert!(ddl.contains(
            "ALTER SEQUENCE \"archive\".\"orders_id_seq\" OWNED BY \"archive\".\"orders\".\"id\";"
        ));
    }

    #[test]
    fn preserves_non_sequence_default_expression() {
        let ddl = create_table_ddl(
            "archive",
            "events",
            &[ColumnDef {
                name: "created_at".to_owned(),
                type_sql: "timestamp with time zone".to_owned(),
                not_null: true,
                default_expr: Some("now()".to_owned()),
            }],
        );

        assert!(!ddl.contains("CREATE SEQUENCE"));
        assert!(ddl.contains("\"created_at\" timestamp with time zone DEFAULT now() NOT NULL"));
    }
}

use std::{collections::HashMap, fmt, fmt::Write as _};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio_postgres::{Client, Config, NoTls, Row};

use crate::sql::{quote_ident, quoted_fq_name};
use crate::types::DataFormat;

/// Тип PostgreSQL-реляции, поддерживаемый инструментом.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    Table,
    View,
    MaterializedView,
}

impl RelationKind {
    /// Стабильное строковое представление для manifest и сообщений.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::View => "view",
            Self::MaterializedView => "materialized_view",
        }
    }
}

impl fmt::Display for RelationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
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

/// Минимальная ссылка на relation в source БД.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationRef {
    pub schema: String,
    pub name: String,
    pub kind: RelationKind,
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
    let rows = query_relation_column_rows(
        client,
        schema,
        name,
        "
        a.attname,
        pg_catalog.format_type(a.atttypid, a.atttypmod) AS type_sql
        ",
        "",
        "typed column list",
    )
    .await?;

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
    let rows = query_relation_column_rows(
        client,
        schema,
        name,
        "
        a.attname,
        pg_catalog.format_type(a.atttypid, a.atttypmod) AS type_sql,
        a.attnotnull,
        pg_catalog.pg_get_expr(ad.adbin, ad.adrelid) AS default_expr
        ",
        "LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum",
        "column definitions",
    )
    .await?;

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

async fn query_relation_column_rows(
    client: &Client,
    schema: &str,
    name: &str,
    select_clause: &str,
    extra_joins: &str,
    context_label: &str,
) -> Result<Vec<Row>> {
    let sql = format!(
        "
        SELECT
            {select_clause}
        FROM pg_attribute a
        JOIN pg_class c ON c.oid = a.attrelid
        JOIN pg_namespace n ON n.oid = c.relnamespace
        {extra_joins}
        WHERE n.nspname = $1
          AND c.relname = $2
          AND a.attnum > 0
          AND NOT a.attisdropped
        ORDER BY a.attnum
        "
    );
    let rows = client
        .query(&sql, &[&schema, &name])
        .await
        .with_context(|| format!("failed to fetch {context_label} for {schema}.{name}"))?;

    if rows.is_empty() {
        bail!("source relation {schema}.{name} has no columns");
    }

    Ok(rows)
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

/// Возвращает SQL-определение view (только тело SELECT) через pg_get_viewdef.
pub async fn view_definition_sql(client: &Client, schema: &str, name: &str) -> Result<String> {
    let row = client
        .query_opt(
            "
            SELECT pg_catalog.pg_get_viewdef(c.oid, true)
            FROM pg_class c
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = $1
              AND c.relname = $2
              AND c.relkind = 'v'
            ",
            &[&schema, &name],
        )
        .await
        .with_context(|| format!("failed to fetch view definition for {schema}.{name}"))?;

    let view_sql = row
        .map(|row| row.get::<_, String>(0))
        .with_context(|| format!("source relation {schema}.{name} is not a view"))?;
    Ok(view_sql)
}

/// Возвращает транзитивные зависимости view по relation-объектам (table/view/mview).
pub async fn view_dependencies_transitive(
    client: &Client,
    schema: &str,
    name: &str,
) -> Result<Vec<RelationRef>> {
    let rows = client
        .query(
            "
            WITH RECURSIVE deps AS (
                SELECT c.oid, n.nspname, c.relname, c.relkind::text, 0::int AS depth
                FROM pg_class c
                JOIN pg_namespace n ON n.oid = c.relnamespace
                WHERE n.nspname = $1
                  AND c.relname = $2
                  AND c.relkind = 'v'

                UNION ALL

                SELECT c_dep.oid, n_dep.nspname, c_dep.relname, c_dep.relkind::text, deps.depth + 1
                FROM deps
                JOIN pg_rewrite rw ON rw.ev_class = deps.oid
                JOIN pg_depend d ON d.classid = 'pg_rewrite'::regclass
                                AND d.objid = rw.oid
                                AND d.refclassid = 'pg_class'::regclass
                                AND d.deptype = 'n'
                JOIN pg_class c_dep ON c_dep.oid = d.refobjid
                JOIN pg_namespace n_dep ON n_dep.oid = c_dep.relnamespace
                WHERE c_dep.relkind IN ('r', 'p', 'f', 'v', 'm')
                  AND c_dep.oid <> deps.oid
            )
            SELECT nspname, relname, relkind
            FROM (
                SELECT
                    oid,
                    nspname,
                    relname,
                    relkind,
                    MAX(depth) AS max_depth
                FROM deps
                WHERE depth > 0
                GROUP BY oid, nspname, relname, relkind
            ) ranked
            ORDER BY max_depth DESC, nspname, relname
            ",
            &[&schema, &name],
        )
        .await
        .with_context(|| format!("failed to resolve dependencies for view {schema}.{name}"))?;

    rows.into_iter()
        .map(|row| {
            let dep_schema = row.get::<_, String>(0);
            let dep_name = row.get::<_, String>(1);
            let relkind = row.get::<_, String>(2);
            let kind = relation_kind_from_relkind(&relkind, &dep_schema, &dep_name)?;
            Ok(RelationRef {
                schema: dep_schema,
                name: dep_name,
                kind,
            })
        })
        .collect::<Result<Vec<_>>>()
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
        writeln!(
            ddl,
            "CREATE SEQUENCE {};",
            quoted_fq_name(target_schema, sequence_name)
        )
        .expect("writing to string is infallible");
    }

    write!(
        ddl,
        "CREATE TABLE {} (\n    {body}\n);\n",
        quoted_fq_name(target_schema, target_name)
    )
    .expect("writing to string is infallible");

    for (sequence_name, column_name) in &sequence_specs {
        writeln!(
            ddl,
            "ALTER SEQUENCE {} OWNED BY {}.{};",
            quoted_fq_name(target_schema, sequence_name),
            quoted_fq_name(target_schema, target_name),
            quote_ident(column_name)
        )
        .expect("writing to string is infallible");
    }

    ddl
}

/// Генерирует DDL view для импорта.
pub fn create_view_ddl(target_schema: &str, target_name: &str, view_sql: &str) -> String {
    let normalized_view_sql = view_sql.trim().trim_end_matches(';');
    format!(
        "CREATE VIEW {} AS\n{normalized_view_sql};\n",
        quoted_fq_name(target_schema, target_name)
    )
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

const fn copy_format_options(data_format: DataFormat) -> &'static str {
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
        ColumnDef, RelationKind, copy_in_sql, copy_out_sql, create_table_ddl, create_view_ddl,
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

    #[test]
    fn builds_view_ddl() {
        let ddl = create_view_ddl(
            "archive",
            "sales_view",
            "SELECT id, amount FROM reporting.sales_daily",
        );
        assert_eq!(
            ddl,
            "CREATE VIEW \"archive\".\"sales_view\" AS\nSELECT id, amount FROM reporting.sales_daily;\n"
        );
    }
}

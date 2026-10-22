mod ddl;

use std::{collections::HashMap, fmt};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio_postgres::{Client, Config, NoTls, Row};

pub use ddl::{copy_in_sql, copy_out_sql, create_table_ddl, create_view_ddl};

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
#[derive(Debug)]
pub struct ColumnDef {
    pub name: String,
    pub type_sql: String,
    pub not_null: bool,
    pub default_expr: Option<String>,
}

/// Минимальная ссылка на relation в source БД.
#[derive(Debug)]
pub struct RelationRef {
    pub schema: String,
    pub name: String,
}

/// Выбирает определения только для эффективного списка колонок.
pub fn pick_column_defs<'a>(
    all_defs: &'a [ColumnDef],
    effective_columns: &[String],
    source_schema: &str,
    source_name: &str,
) -> Result<Vec<&'a ColumnDef>> {
    let by_name = all_defs
        .iter()
        .map(|definition| (definition.name.as_str(), definition))
        .collect::<HashMap<_, _>>();

    effective_columns
        .iter()
        .map(|column| {
            by_name.get(column.as_str()).copied().with_context(|| {
                format!("column definition for {source_schema}.{source_name}.{column} is missing")
            })
        })
        .collect()
}

fn relation_kind_from_relkind(relkind: &str, schema: &str, name: &str) -> Result<RelationKind> {
    match relkind {
        "r" | "p" | "f" => Ok(RelationKind::Table),
        "v" => Ok(RelationKind::View),
        "m" => Ok(RelationKind::MaterializedView),
        other => bail!("unsupported relation kind '{other}' for {schema}.{name}"),
    }
}

/// Устанавливает подключение к `PostgreSQL` и запускает background-task драйвера.
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

/// Возвращает SQL-определение view (только тело SELECT) через `pg_get_viewdef`.
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
            SELECT nspname, relname
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

    Ok(rows
        .into_iter()
        .map(|row| RelationRef {
            schema: row.get(0),
            name: row.get(1),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{RelationKind, relation_kind_from_relkind};

    #[test]
    fn maps_materialized_view_relkind() {
        let kind = relation_kind_from_relkind("m", "reporting", "sales_mv")
            .expect("materialized view relkind should be supported");
        assert_eq!(kind, RelationKind::MaterializedView);
        assert_eq!(kind.as_str(), "materialized_view");
    }
}

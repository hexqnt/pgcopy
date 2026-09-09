use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;

use anyhow::{Context, Result, bail};
use tokio_postgres::{Client, Row};

use crate::pg::RelationKind;

use super::{
    ColumnDescription, ColumnStatistics, CommonValue, ConstraintDescription, ConstraintKind,
    DatabaseDescription, GeneratedColumn, GeneratedStorage, IdentityGeneration,
    RelationDescription, RelationName, SourceRelation, StatisticsMode, StatisticsStatus,
};

pub(super) struct CatalogOptions {
    pub statistics: StatisticsMode,
    pub include_comments: bool,
}

fn relation_kind(relkind: &str, source: SourceRelation<'_>) -> Result<RelationKind> {
    match relkind {
        "r" | "p" | "f" => Ok(RelationKind::Table),
        "v" => Ok(RelationKind::View),
        "m" => Ok(RelationKind::MaterializedView),
        other => bail!(
            "unsupported relation kind '{other}' for {}.{}",
            source.schema,
            source.name
        ),
    }
}

fn parse_column(row: &Row) -> Result<ColumnDescription> {
    let mut expression: Option<String> = row.get(3);
    let identity_raw: String = row.get(4);
    let generated_raw: String = row.get(5);
    let identity = match identity_raw.as_str() {
        "" => None,
        "a" => Some(IdentityGeneration::Always),
        "d" => Some(IdentityGeneration::ByDefault),
        other => bail!("unsupported PostgreSQL identity kind '{other}'"),
    };
    let generated = match generated_raw.as_str() {
        "" => None,
        "s" | "v" => Some(GeneratedColumn {
            expression: expression
                .take()
                .context("generated column is missing its expression")?,
            storage: if generated_raw == "s" {
                GeneratedStorage::Stored
            } else {
                GeneratedStorage::Virtual
            },
        }),
        other => bail!("unsupported PostgreSQL generated column kind '{other}'"),
    };
    let enum_values_json: String = row.get(8);
    let enum_values = serde_json::from_str(&enum_values_json)
        .context("failed to decode PostgreSQL enum labels")?;
    let default_expression = if identity.is_none() && generated.is_none() {
        expression
    } else {
        None
    };

    Ok(ColumnDescription {
        name: row.get(0),
        type_sql: row.get(1),
        nullable: !row.get::<_, bool>(2),
        default_expression,
        identity,
        generated,
        comment: row.get(6),
        enum_values,
        statistics: None,
        binary: row.get(7),
    })
}

fn parse_common_values(row: &Row, limit: Option<NonZeroUsize>) -> Result<Vec<CommonValue>> {
    let Some(limit) = limit else {
        return Ok(Vec::new());
    };
    let Some(values_json) = row.get::<_, Option<String>>(4) else {
        return Ok(Vec::new());
    };
    let frequencies_json = row
        .get::<_, Option<String>>(5)
        .context("pg_stats has common values without frequencies")?;
    let values: Vec<serde_json::Value> =
        serde_json::from_str(&values_json).context("failed to decode pg_stats common values")?;
    let frequencies: Vec<f64> = serde_json::from_str(&frequencies_json)
        .context("failed to decode pg_stats common frequencies")?;
    if values.len() != frequencies.len() {
        bail!(
            "pg_stats common values/frequencies length mismatch: {} values, {} frequencies",
            values.len(),
            frequencies.len()
        );
    }
    Ok(values
        .into_iter()
        .zip(frequencies)
        .take(limit.get())
        .map(|(value, frequency)| CommonValue { value, frequency })
        .collect())
}

fn apply_statistics(
    columns: &mut [ColumnDescription],
    mut statistics: HashMap<String, ColumnStatistics>,
) -> bool {
    let mut available = false;
    for column in columns {
        column.statistics = statistics.remove(&column.name);
        available |= column.statistics.is_some();
        if column.binary
            && let Some(column_statistics) = column.statistics.as_mut()
        {
            column_statistics.most_common_values.clear();
        }
    }
    available
}

pub(super) async fn load(
    client: &Client,
    sources: &[SourceRelation<'_>],
    options: CatalogOptions,
) -> Result<DatabaseDescription> {
    client
        .batch_execute(
            "BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY;\n\
             SET LOCAL statement_timeout = '30s';\n\
             SET LOCAL search_path = ''",
        )
        .await
        .context("failed to begin read-only schema description transaction")?;

    let result = load_in_transaction(client, sources, options).await;
    match result {
        Ok(description) => {
            client
                .batch_execute("COMMIT")
                .await
                .context("failed to commit schema description transaction")?;
            Ok(description)
        }
        Err(error) => {
            let _ = client.batch_execute("ROLLBACK").await;
            Err(error)
        }
    }
}

async fn load_in_transaction(
    client: &Client,
    sources: &[SourceRelation<'_>],
    options: CatalogOptions,
) -> Result<DatabaseDescription> {
    let version_row = client
        .query_one(
            "SELECT current_setting('server_version'), current_setting('server_version_num')",
            &[],
        )
        .await
        .context("failed to fetch PostgreSQL version")?;
    let postgres_version = version_row.get(0);
    let version_num_raw: String = version_row.get(1);
    let postgres_version_num = version_num_raw
        .parse()
        .with_context(|| format!("invalid server_version_num '{version_num_raw}'"))?;

    let selected = sources
        .iter()
        .map(|source| (source.schema, source.name))
        .collect::<HashSet<_>>();
    let mut relations = Vec::with_capacity(sources.len());
    for source in sources {
        relations.push(load_relation(client, *source, &options).await?);
    }

    let mut warnings = Vec::new();
    for relation in &relations {
        for constraint in &relation.constraints {
            let Some(reference) = constraint.referenced_relation.as_ref() else {
                continue;
            };
            if !selected.contains(&(reference.schema.as_str(), reference.name.as_str())) {
                warnings.push(format!(
                    "Foreign key {} on {}.{} references {}.{}, which is not selected by the config",
                    constraint.name,
                    relation.schema,
                    relation.name,
                    reference.schema,
                    reference.name,
                ));
            }
        }
    }

    Ok(DatabaseDescription {
        format_version: 1,
        postgres_version,
        postgres_version_num,
        profile: options.statistics.profile(),
        relations,
        warnings,
    })
}

async fn load_relation(
    client: &Client,
    source: SourceRelation<'_>,
    options: &CatalogOptions,
) -> Result<RelationDescription> {
    let relation_row = client
        .query_opt(
            "
            SELECT
                c.relkind::text,
                CASE WHEN $3 THEN pg_catalog.obj_description(c.oid, 'pg_class') END,
                CASE WHEN $4 AND c.relkind <> 'v' AND c.reltuples >= 0
                     THEN c.reltuples::bigint END,
                CASE WHEN $4 AND c.relkind <> 'v' AND c.reltuples >= 0
                     THEN c.reltuples::float8 END
            FROM pg_catalog.pg_class c
            JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = $1 AND c.relname = $2
            ",
            &[
                &source.schema,
                &source.name,
                &options.include_comments,
                &options.statistics.is_enabled(),
            ],
        )
        .await
        .with_context(|| {
            format!(
                "failed to fetch relation metadata for {}.{}",
                source.schema, source.name
            )
        })?
        .with_context(|| {
            format!(
                "source relation {}.{} does not exist",
                source.schema, source.name
            )
        })?;

    let relkind: String = relation_row.get(0);
    let kind = relation_kind(&relkind, source)?;
    let comment = relation_row.get(1);
    let row_estimate = relation_row.get(2);
    let row_estimate_float = relation_row.get(3);
    let mut columns = load_columns(client, source, options.include_comments).await?;
    let constraints = load_constraints(client, source).await?;
    let view_definition = load_view_definition(client, source, kind).await?;

    let statistics_status = if options.statistics.is_enabled() {
        let statistics = load_statistics(
            client,
            source,
            row_estimate_float,
            options.statistics.top_values(),
        )
        .await?;
        let available = apply_statistics(&mut columns, statistics);
        Some(if available {
            StatisticsStatus::Available
        } else {
            StatisticsStatus::Unavailable
        })
    } else {
        None
    };

    Ok(RelationDescription {
        schema: source.schema.to_owned(),
        name: source.name.to_owned(),
        kind,
        comment,
        columns,
        constraints,
        view_definition,
        row_estimate,
        statistics_status,
    })
}

async fn load_columns(
    client: &Client,
    source: SourceRelation<'_>,
    include_comments: bool,
) -> Result<Vec<ColumnDescription>> {
    let rows = client
        .query(
            "
            SELECT
                a.attname,
                pg_catalog.format_type(a.atttypid, a.atttypmod),
                a.attnotnull,
                pg_catalog.pg_get_expr(ad.adbin, ad.adrelid),
                a.attidentity::text,
                a.attgenerated::text,
                CASE WHEN $3 THEN pg_catalog.col_description(c.oid, a.attnum) END,
                CASE WHEN t.typtype = 'd' THEN t.typbasetype ELSE t.oid END =
                    'pg_catalog.bytea'::pg_catalog.regtype,
                COALESCE((
                    SELECT pg_catalog.json_agg(e.enumlabel ORDER BY e.enumsortorder)
                    FROM pg_catalog.pg_enum e
                    WHERE e.enumtypid = CASE WHEN t.typtype = 'd'
                                             THEN t.typbasetype ELSE a.atttypid END
                ), '[]'::json)::text
            FROM pg_catalog.pg_attribute a
            JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
            JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
            JOIN pg_catalog.pg_type t ON t.oid = a.atttypid
            LEFT JOIN pg_catalog.pg_attrdef ad
              ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
            WHERE n.nspname = $1
              AND c.relname = $2
              AND a.attnum > 0
              AND NOT a.attisdropped
            ORDER BY a.attnum
            ",
            &[&source.schema, &source.name, &include_comments],
        )
        .await
        .with_context(|| {
            format!(
                "failed to fetch column metadata for {}.{}",
                source.schema, source.name
            )
        })?;

    if rows.is_empty() {
        bail!(
            "source relation {}.{} has no columns",
            source.schema,
            source.name
        );
    }

    rows.iter().map(parse_column).collect()
}

async fn load_constraints(
    client: &Client,
    source: SourceRelation<'_>,
) -> Result<Vec<ConstraintDescription>> {
    let rows = client
        .query(
            "
            SELECT
                con.conname,
                con.contype::text,
                pg_catalog.pg_get_constraintdef(con.oid, true),
                foreign_ns.nspname,
                foreign_class.relname
            FROM pg_catalog.pg_constraint con
            JOIN pg_catalog.pg_class c ON c.oid = con.conrelid
            JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
            LEFT JOIN pg_catalog.pg_class foreign_class ON foreign_class.oid = con.confrelid
            LEFT JOIN pg_catalog.pg_namespace foreign_ns
              ON foreign_ns.oid = foreign_class.relnamespace
            WHERE n.nspname = $1
              AND c.relname = $2
              AND con.contype IN ('p', 'u', 'f', 'c', 'x')
            ORDER BY CASE con.contype
                         WHEN 'p' THEN 1 WHEN 'u' THEN 2 WHEN 'f' THEN 3
                         WHEN 'c' THEN 4 ELSE 5
                     END,
                     con.conname
            ",
            &[&source.schema, &source.name],
        )
        .await
        .with_context(|| {
            format!(
                "failed to fetch constraints for {}.{}",
                source.schema, source.name
            )
        })?;

    rows.into_iter()
        .map(|row| {
            let raw_kind: String = row.get(1);
            let kind = match raw_kind.as_str() {
                "p" => ConstraintKind::PrimaryKey,
                "u" => ConstraintKind::Unique,
                "f" => ConstraintKind::ForeignKey,
                "c" => ConstraintKind::Check,
                "x" => ConstraintKind::Exclusion,
                _ => bail!("unsupported constraint kind '{raw_kind}'"),
            };
            let referenced_schema: Option<String> = row.get(3);
            let referenced_name: Option<String> = row.get(4);
            let referenced_relation = referenced_schema
                .zip(referenced_name)
                .map(|(schema, name)| RelationName { schema, name });
            Ok(ConstraintDescription {
                name: row.get(0),
                kind,
                definition: row.get(2),
                referenced_relation,
            })
        })
        .collect()
}

async fn load_view_definition(
    client: &Client,
    source: SourceRelation<'_>,
    kind: RelationKind,
) -> Result<Option<String>> {
    if kind == RelationKind::Table {
        return Ok(None);
    }
    let row = client
        .query_one(
            "
            SELECT pg_catalog.pg_get_viewdef(c.oid, true)
            FROM pg_catalog.pg_class c
            JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('v', 'm')
            ",
            &[&source.schema, &source.name],
        )
        .await
        .with_context(|| {
            format!(
                "failed to fetch view definition for {}.{}",
                source.schema, source.name
            )
        })?;
    Ok(Some(row.get(0)))
}

async fn load_statistics(
    client: &Client,
    source: SourceRelation<'_>,
    row_estimate: Option<f64>,
    top_values: Option<NonZeroUsize>,
) -> Result<HashMap<String, ColumnStatistics>> {
    let common_values_select = if top_values.is_none() {
        "NULL::text, NULL::text"
    } else {
        "pg_catalog.to_json(most_common_vals)::text, \
         pg_catalog.to_json(most_common_freqs)::text"
    };
    let sql = format!(
        "
        SELECT
            attname,
            null_frac::float8,
            n_distinct::float8,
            avg_width,
            {common_values_select}
        FROM pg_catalog.pg_stats
        WHERE schemaname = $1 AND tablename = $2
        ORDER BY attname
        "
    );
    let rows = client
        .query(&sql, &[&source.schema, &source.name])
        .await
        .with_context(|| {
            format!(
                "failed to fetch planner statistics for {}.{}",
                source.schema, source.name
            )
        })?;

    rows.into_iter()
        .map(|row| {
            let name: String = row.get(0);
            let n_distinct: f64 = row.get(2);
            let estimated_distinct = if n_distinct >= 0.0 {
                Some(n_distinct)
            } else {
                row_estimate.map(|rows| -n_distinct * rows)
            };
            let most_common_values = parse_common_values(&row, top_values)?;
            Ok((
                name,
                ColumnStatistics {
                    null_fraction: row.get(1),
                    estimated_distinct,
                    average_width_bytes: row.get(3),
                    most_common_values,
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::apply_statistics;
    use crate::describe::{ColumnDescription, ColumnStatistics, CommonValue};

    #[test]
    fn applies_statistics_by_column_name() {
        let mut columns = vec![ColumnDescription {
            name: "id".to_owned(),
            type_sql: "bigint".to_owned(),
            nullable: false,
            default_expression: None,
            identity: None,
            generated: None,
            comment: None,
            enum_values: Vec::new(),
            statistics: None,
            binary: true,
        }];
        let statistics = HashMap::from([(
            "id".to_owned(),
            ColumnStatistics {
                null_fraction: 0.0,
                estimated_distinct: Some(42.0),
                average_width_bytes: 8,
                most_common_values: vec![CommonValue {
                    value: serde_json::Value::String("sensitive bytes".to_owned()),
                    frequency: 1.0,
                }],
            },
        )]);

        assert!(apply_statistics(&mut columns, statistics));
        assert_eq!(
            columns[0]
                .statistics
                .as_ref()
                .and_then(|value| value.estimated_distinct),
            Some(42.0)
        );
        assert!(
            columns[0]
                .statistics
                .as_ref()
                .is_some_and(|value| value.most_common_values.is_empty())
        );
    }
}

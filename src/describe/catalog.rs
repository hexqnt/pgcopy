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

mod sql;

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
    let selected_names = sources
        .iter()
        .map(|source| (source.schema, source.name))
        .collect::<HashSet<_>>();
    let selection = CatalogSelection::new(sources);

    // Независимые пакетные запросы одновременно передаются драйверу. PostgreSQL
    // выполняет их последовательно в одной транзакции, но без round-trip на объект.
    let (version, metadata, columns, constraints, statistics) = tokio::try_join!(
        load_version(client),
        load_relation_metadata(client, sources, &selection, &options),
        load_columns(client, &selection, options.include_comments),
        load_constraints(client, &selection),
        load_statistics(client, &selection, options.statistics),
    )?;

    let parts = metadata
        .into_iter()
        .zip(columns)
        .zip(constraints)
        .zip(statistics)
        .map(
            |(((metadata, columns), constraints), statistics)| RelationParts {
                metadata,
                columns,
                constraints,
                statistics,
            },
        );
    let relations = sources
        .iter()
        .zip(parts)
        .map(|(source, parts)| parts.into_description(*source, options.statistics.is_enabled()))
        .collect::<Result<Vec<_>>>()?;

    let mut warnings = Vec::new();
    for relation in &relations {
        for constraint in &relation.constraints {
            let Some(reference) = constraint.referenced_relation.as_ref() else {
                continue;
            };
            if !selected_names.contains(&(reference.schema.as_str(), reference.name.as_str())) {
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
        postgres_version: version.text,
        postgres_version_num: version.number,
        profile: options.statistics.profile(),
        relations,
        warnings,
    })
}

struct PostgresVersion {
    text: String,
    number: i32,
}

async fn load_version(client: &Client) -> Result<PostgresVersion> {
    let row = client
        .query_one(
            "SELECT current_setting('server_version'), current_setting('server_version_num')",
            &[],
        )
        .await
        .context("failed to fetch PostgreSQL version")?;
    let text = row.get(0);
    let number_raw: String = row.get(1);
    let number = number_raw
        .parse()
        .with_context(|| format!("invalid server_version_num '{number_raw}'"))?;
    Ok(PostgresVersion { text, number })
}

struct RelationMetadata {
    kind: RelationKind,
    comment: Option<String>,
    view_definition: Option<String>,
    row_estimate: Option<i64>,
}

struct RelationParts {
    metadata: RelationMetadata,
    columns: Vec<ColumnDescription>,
    constraints: Vec<ConstraintDescription>,
    statistics: HashMap<String, ColumnStatistics>,
}

impl RelationParts {
    fn into_description(
        mut self,
        source: SourceRelation<'_>,
        statistics_enabled: bool,
    ) -> Result<RelationDescription> {
        if self.columns.is_empty() {
            bail!(
                "source relation {}.{} has no columns",
                source.schema,
                source.name
            );
        }
        let statistics_status = statistics_enabled.then(|| {
            if apply_statistics(&mut self.columns, self.statistics) {
                StatisticsStatus::Available
            } else {
                StatisticsStatus::Unavailable
            }
        });
        Ok(RelationDescription {
            schema: source.schema.to_owned(),
            name: source.name.to_owned(),
            kind: self.metadata.kind,
            comment: self.metadata.comment,
            columns: self.columns,
            constraints: self.constraints,
            view_definition: self.metadata.view_definition,
            row_estimate: self.metadata.row_estimate,
            statistics_status,
        })
    }
}

struct CatalogSelection<'a> {
    schemas: Vec<&'a str>,
    relation_names: Vec<&'a str>,
}

impl<'a> CatalogSelection<'a> {
    fn new(sources: &[SourceRelation<'a>]) -> Self {
        Self {
            schemas: sources.iter().map(|source| source.schema).collect(),
            relation_names: sources.iter().map(|source| source.name).collect(),
        }
    }

    const fn len(&self) -> usize {
        self.schemas.len()
    }
}

fn source_index(row: &Row, source_count: usize) -> Result<usize> {
    let ordinal: i64 = row
        .try_get("source_index")
        .context("PostgreSQL catalog result has no valid source ordinal")?;
    usize::try_from(ordinal)
        .ok()
        .and_then(|value| value.checked_sub(1))
        .filter(|&index| index < source_count)
        .with_context(|| format!("PostgreSQL returned invalid source ordinal {ordinal}"))
}

async fn load_relation_metadata(
    client: &Client,
    sources: &[SourceRelation<'_>],
    selection: &CatalogSelection<'_>,
    options: &CatalogOptions,
) -> Result<Vec<RelationMetadata>> {
    let rows = client
        .query(
            sql::RELATION_METADATA,
            &[
                &selection.schemas,
                &selection.relation_names,
                &options.include_comments,
                &options.statistics.is_enabled(),
            ],
        )
        .await
        .context("failed to fetch relation metadata")?;

    let mut metadata = (0..sources.len()).map(|_| None).collect::<Vec<_>>();
    for row in rows {
        let index = source_index(&row, sources.len())?;
        let source = sources[index];
        let relkind = row.get::<_, Option<String>>(0).with_context(|| {
            format!(
                "source relation {}.{} does not exist",
                source.schema, source.name
            )
        })?;
        metadata[index] = Some(RelationMetadata {
            kind: relation_kind(&relkind, source)?,
            comment: row.get(1),
            row_estimate: row.get(2),
            view_definition: row.get(3),
        });
    }

    metadata
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            item.with_context(|| {
                let source = sources[index];
                format!(
                    "source relation {}.{} is missing from the catalog result",
                    source.schema, source.name
                )
            })
        })
        .collect()
}

async fn load_columns(
    client: &Client,
    selection: &CatalogSelection<'_>,
    include_comments: bool,
) -> Result<Vec<Vec<ColumnDescription>>> {
    let rows = client
        .query(
            sql::COLUMNS,
            &[
                &selection.schemas,
                &selection.relation_names,
                &include_comments,
            ],
        )
        .await
        .context("failed to fetch column metadata")?;

    let mut columns = (0..selection.len()).map(|_| Vec::new()).collect::<Vec<_>>();
    for row in rows {
        let index = source_index(&row, selection.len())?;
        columns[index].push(parse_column(&row)?);
    }
    Ok(columns)
}

async fn load_constraints(
    client: &Client,
    selection: &CatalogSelection<'_>,
) -> Result<Vec<Vec<ConstraintDescription>>> {
    let rows = client
        .query(
            sql::CONSTRAINTS,
            &[&selection.schemas, &selection.relation_names],
        )
        .await
        .context("failed to fetch constraints")?;

    let mut constraints = (0..selection.len()).map(|_| Vec::new()).collect::<Vec<_>>();
    for row in rows {
        let index = source_index(&row, selection.len())?;
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
        constraints[index].push(ConstraintDescription {
            name: row.get(0),
            kind,
            definition: row.get(2),
            referenced_relation,
        });
    }
    Ok(constraints)
}

async fn load_statistics(
    client: &Client,
    selection: &CatalogSelection<'_>,
    mode: StatisticsMode,
) -> Result<Vec<HashMap<String, ColumnStatistics>>> {
    let mut statistics = (0..selection.len())
        .map(|_| HashMap::new())
        .collect::<Vec<_>>();
    if !mode.is_enabled() {
        return Ok(statistics);
    }
    let sql = if mode.top_values().is_some() {
        sql::STATISTICS_WITH_COMMON_VALUES
    } else {
        sql::STATISTICS
    };
    let rows = client
        .query(sql, &[&selection.schemas, &selection.relation_names])
        .await
        .context("failed to fetch planner statistics")?;

    let top_values = mode.top_values();
    for row in rows {
        let index = source_index(&row, selection.len())?;
        statistics[index].insert(
            row.get(0),
            ColumnStatistics {
                null_fraction: row.get(1),
                estimated_distinct: row.get(2),
                average_width_bytes: row.get(3),
                most_common_values: parse_common_values(&row, top_values)?,
            },
        );
    }
    Ok(statistics)
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

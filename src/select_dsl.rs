use std::{collections::HashSet, fmt};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::sql::quote_ident;

mod clauses;
mod ident;

/// Поддерживаемый тип проекции в select DSL.
#[derive(Debug, Clone)]
pub enum ColumnProjection {
    All,
    ColumnsList(Vec<String>),
    ExceptList(Vec<String>),
}

/// Стабильный код типа проекции для manifest.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProjectionKind {
    #[serde(rename = "*")]
    All,
    #[serde(rename = "columns_list")]
    ColumnsList,
    #[serde(rename = "except_list")]
    ExceptList,
}

impl ProjectionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "*",
            Self::ColumnsList => "columns_list",
            Self::ExceptList => "except_list",
        }
    }
}

impl fmt::Display for ProjectionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Ограниченный DSL для безопасного описания источника выборки.
///
/// Поддерживаемый хвост: `WHERE`, `ORDER BY`, `LIMIT` (строго в этом порядке).
#[derive(Debug, Clone)]
pub struct SelectDsl {
    pub raw: String,
    pub source_schema: String,
    pub source_name: String,
    pub projection: ColumnProjection,
    pub where_clause: Option<String>,
    pub order_by_clause: Option<String>,
    pub limit_clause: Option<u64>,
}

impl SelectDsl {
    /// Парсит строку DSL в структурированную форму.
    pub fn parse(input: &str) -> Result<Self> {
        let trimmed = input.trim();
        let after_select =
            clauses::strip_leading_keyword(trimmed, "select").with_context(|| {
                format!("unsupported select DSL syntax '{input}'. Query must start with SELECT")
            })?;

        let (projection_raw, from_tail) = clauses::split_on_keyword(after_select, "from")
            .with_context(|| {
                format!("unsupported select DSL syntax '{input}'. Missing FROM <schema>.<object>")
            })?;

        let projection = parse_projection(projection_raw)?;
        let (source_schema, source_name, clauses_tail) = ident::parse_source_relation(from_tail)?;
        let clauses = clauses::parse_optional_clauses(clauses_tail)?;

        Ok(Self {
            raw: trimmed.to_owned(),
            source_schema,
            source_name,
            projection,
            where_clause: clauses.where_clause,
            order_by_clause: clauses.order_by_clause,
            limit_clause: clauses.limit_clause,
        })
    }

    /// Возвращает тип проекции в виде стабильного кода.
    pub const fn projection_kind(&self) -> ProjectionKind {
        match self.projection {
            ColumnProjection::All => ProjectionKind::All,
            ColumnProjection::ColumnsList(_) => ProjectionKind::ColumnsList,
            ColumnProjection::ExceptList(_) => ProjectionKind::ExceptList,
        }
    }

    /// Строит итоговый список колонок с валидацией against source schema.
    pub fn effective_columns(&self, source_columns: &[String]) -> Result<Vec<String>> {
        let source_set: HashSet<&str> = source_columns.iter().map(String::as_str).collect();

        let effective = match &self.projection {
            ColumnProjection::All => source_columns.to_vec(),
            ColumnProjection::ColumnsList(requested) => {
                let mut seen = HashSet::new();
                for column in requested {
                    if !source_set.contains(column.as_str()) {
                        bail!(
                            "column '{column}' from '{}' does not exist in source {}.{}",
                            self.raw,
                            self.source_schema,
                            self.source_name
                        );
                    }
                    if !seen.insert(column) {
                        bail!(
                            "column '{column}' is duplicated in select list '{}'",
                            self.raw
                        );
                    }
                }
                requested.clone()
            }
            ColumnProjection::ExceptList(excluded) => {
                let mut seen = HashSet::new();
                for column in excluded {
                    if !source_set.contains(column.as_str()) {
                        bail!(
                            "column '{column}' from '{}' does not exist in source {}.{}",
                            self.raw,
                            self.source_schema,
                            self.source_name
                        );
                    }
                    if !seen.insert(column) {
                        bail!(
                            "column '{column}' is duplicated in except list '{}'",
                            self.raw
                        );
                    }
                }

                source_columns
                    .iter()
                    // Сохраняем порядок колонок источника для стабильного COPY.
                    .filter(|column| !seen.contains(column))
                    .cloned()
                    .collect::<Vec<_>>()
            }
        };

        if effective.is_empty() {
            bail!(
                "effective column list is empty after applying projection for {}.{}",
                self.source_schema,
                self.source_name
            );
        }

        let mut unique = HashSet::new();
        for column in &effective {
            if !unique.insert(column) {
                bail!(
                    "effective column list has duplicates for {}.{}",
                    self.source_schema,
                    self.source_name
                );
            }
        }

        Ok(effective)
    }

    /// Генерирует канонический SQL `SELECT` для COPY OUT.
    pub fn normalized_select_sql(&self, effective_columns: &[String]) -> String {
        let columns = effective_columns
            .iter()
            .map(|column| quote_ident(column))
            .collect::<Vec<_>>()
            .join(", ");

        let mut select_sql = format!(
            "SELECT {columns} FROM {}.{}",
            quote_ident(&self.source_schema),
            quote_ident(&self.source_name),
        );

        if let Some(where_clause) = self.where_clause.as_deref() {
            select_sql.push_str(" WHERE ");
            select_sql.push_str(where_clause);
        }

        if let Some(order_by_clause) = self.order_by_clause.as_deref() {
            select_sql.push_str(" ORDER BY ");
            select_sql.push_str(order_by_clause);
        }

        if let Some(limit_clause) = self.limit_clause {
            select_sql.push_str(" LIMIT ");
            select_sql.push_str(&limit_clause.to_string());
        }

        select_sql
    }
}

fn parse_projection(raw: &str) -> Result<ColumnProjection> {
    let projection = raw.trim();
    if projection.is_empty() {
        bail!("SELECT list must not be empty");
    }

    if projection == "*" {
        return Ok(ColumnProjection::All);
    }

    if let Some(after_star) = projection.strip_prefix('*') {
        let after_star = after_star.trim_start();
        let after_except = clauses::strip_leading_keyword(after_star, "except")
            .with_context(|| format!("unsupported projection '{projection}'"))?;
        let columns = ident::parse_parenthesized_identifier_list(after_except)?;
        return Ok(ColumnProjection::ExceptList(columns));
    }

    Ok(ColumnProjection::ColumnsList(ident::parse_identifier_list(
        projection,
    )?))
}

#[cfg(test)]
mod tests {
    use super::{ColumnProjection, SelectDsl};

    #[test]
    fn parses_all_projection() {
        let parsed = SelectDsl::parse("select * from public.orders").expect("must parse");
        assert_eq!(parsed.source_schema, "public");
        assert_eq!(parsed.source_name, "orders");
        assert!(matches!(parsed.projection, ColumnProjection::All));
        assert!(parsed.where_clause.is_none());
        assert!(parsed.order_by_clause.is_none());
        assert!(parsed.limit_clause.is_none());
    }

    #[test]
    fn parses_where_clause() {
        let parsed =
            SelectDsl::parse("select * from public.orders where id > 10").expect("must parse");
        assert_eq!(parsed.where_clause.as_deref(), Some("id > 10"));
        assert!(parsed.order_by_clause.is_none());
        assert!(parsed.limit_clause.is_none());
    }

    #[test]
    fn parses_order_by_and_limit() {
        let parsed = SelectDsl::parse(
            "select * from public.orders where id > 10 order by created_at desc, id limit 50",
        )
        .expect("must parse");
        assert_eq!(parsed.where_clause.as_deref(), Some("id > 10"));
        assert_eq!(
            parsed.order_by_clause.as_deref(),
            Some("created_at desc, id")
        );
        assert_eq!(parsed.limit_clause, Some(50));
    }

    #[test]
    fn parses_quoted_schema_starting_with_digit() {
        let parsed = SelectDsl::parse("select * from \"123schema\".\"Orders\"")
            .expect("must parse quoted source relation");
        assert_eq!(parsed.source_schema, "123schema");
        assert_eq!(parsed.source_name, "Orders");
    }

    #[test]
    fn parses_unquoted_schema_starting_with_digit() {
        let parsed = SelectDsl::parse(
            "select * from 13443_schema.v_transgas_regions_gas_temp order by series_name, time_index",
        )
        .expect("must parse source relation with unquoted numeric-leading schema");
        assert_eq!(parsed.source_schema, "13443_schema");
        assert_eq!(parsed.source_name, "v_transgas_regions_gas_temp");
        assert_eq!(
            parsed.order_by_clause.as_deref(),
            Some("series_name, time_index")
        );
    }

    #[test]
    fn parses_quoted_column_list() {
        let parsed = SelectDsl::parse("select \"OrderID\", created_at from public.orders")
            .expect("must parse quoted column list");
        match parsed.projection {
            ColumnProjection::ColumnsList(columns) => {
                assert_eq!(columns, vec!["OrderID".to_owned(), "created_at".to_owned()]);
            }
            _ => panic!("wrong projection kind"),
        }
    }

    #[test]
    fn parses_except_projection() {
        let parsed =
            SelectDsl::parse("select * except (raw_payload, debug_note) from public.orders")
                .expect("must parse");
        match parsed.projection {
            ColumnProjection::ExceptList(columns) => {
                assert_eq!(
                    columns,
                    vec!["raw_payload".to_owned(), "debug_note".to_owned()]
                );
            }
            _ => panic!("wrong projection kind"),
        }
    }

    #[test]
    fn parses_except_projection_with_quoted_column() {
        let parsed =
            SelectDsl::parse("select * except (\"Debug Column\", raw_payload) from public.orders")
                .expect("must parse");
        match parsed.projection {
            ColumnProjection::ExceptList(columns) => {
                assert_eq!(
                    columns,
                    vec!["Debug Column".to_owned(), "raw_payload".to_owned()]
                );
            }
            _ => panic!("wrong projection kind"),
        }
    }

    #[test]
    fn builds_effective_columns_for_except_projection() {
        let parsed =
            SelectDsl::parse("select * except (debug) from public.orders").expect("must parse");
        let effective = parsed
            .effective_columns(&["id".to_owned(), "debug".to_owned(), "status".to_owned()])
            .expect("must build effective list");
        assert_eq!(effective, vec!["id".to_owned(), "status".to_owned()]);
    }

    #[test]
    fn builds_normalized_select_with_where() {
        let parsed = SelectDsl::parse(
            "select id, status from public.orders where status = 'paid' and id > 100",
        )
        .expect("must parse");
        let sql = parsed.normalized_select_sql(&["id".to_owned(), "status".to_owned()]);
        assert_eq!(
            sql,
            "SELECT \"id\", \"status\" FROM \"public\".\"orders\" WHERE status = 'paid' and id > 100"
        );
    }

    #[test]
    fn builds_normalized_select_with_where_order_by_limit() {
        let parsed = SelectDsl::parse(
            "select id, status from public.orders where status = 'paid' order by created_at desc limit 100",
        )
        .expect("must parse");
        let sql = parsed.normalized_select_sql(&["id".to_owned(), "status".to_owned()]);
        assert_eq!(
            sql,
            "SELECT \"id\", \"status\" FROM \"public\".\"orders\" WHERE status = 'paid' ORDER BY created_at desc LIMIT 100"
        );
    }

    #[test]
    fn builds_normalized_select_with_quoted_identifiers() {
        let parsed = SelectDsl::parse("select \"OrderID\" from \"123schema\".\"Orders\"")
            .expect("must parse");
        let sql = parsed.normalized_select_sql(&["OrderID".to_owned()]);
        assert_eq!(sql, "SELECT \"OrderID\" FROM \"123schema\".\"Orders\"");
    }

    #[test]
    fn rejects_where_with_semicolon() {
        let error = SelectDsl::parse("select * from public.orders where id > 10; drop table t")
            .expect_err("must reject semicolon");
        assert!(error.to_string().contains("must not contain ';'"));
    }

    #[test]
    fn rejects_invalid_limit_value() {
        let error = SelectDsl::parse("select * from public.orders limit ten")
            .expect_err("must reject invalid limit");
        assert!(error.to_string().contains("invalid LIMIT value"));
    }

    #[test]
    fn rejects_order_by_after_limit() {
        let error = SelectDsl::parse("select * from public.orders limit 10 order by id desc")
            .expect_err("must reject ORDER BY after LIMIT");
        assert!(
            error
                .to_string()
                .contains("unsupported trailing clause after LIMIT")
        );
    }

    #[test]
    fn rejects_unsupported_sql_constructs() {
        let error = SelectDsl::parse("select * from public.orders join public.users on true")
            .expect_err("must reject unsupported syntax");
        assert!(
            error
                .to_string()
                .contains("unsupported trailing clause in select DSL")
        );
    }

    #[test]
    fn rejects_unquoted_column_starting_with_digit() {
        let error = SelectDsl::parse("select 1col from public.orders")
            .expect_err("must reject unquoted numeric-leading column identifier");
        assert!(error.to_string().contains("invalid identifier in '1col'"));
    }
}

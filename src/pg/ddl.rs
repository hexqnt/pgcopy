use std::fmt::Write as _;

use crate::sql::{push_quoted_ident_list, quote_ident, quoted_fq_name};
use crate::types::DataFormat;

use super::ColumnDef;

/// Генерирует DDL целевой таблицы для импорта.
///
/// Отдельно создаёт локальные sequence для serial-like default выражений,
/// чтобы не ссылаться на sequence исходной БД.
pub fn create_table_ddl<'a>(
    target_schema: &str,
    target_name: &str,
    columns: impl Iterator<Item = &'a ColumnDef> + Clone,
) -> String {
    let target = quoted_fq_name(target_schema, target_name);
    let sequence_columns = columns.clone().filter(|column| {
        column
            .default_expr
            .as_deref()
            .is_some_and(is_regclass_nextval_default)
    });
    let sequence_name = |column: &ColumnDef| {
        quoted_fq_name(target_schema, &format!("{target_name}_{}_seq", column.name))
    };
    let mut ddl = String::new();
    for column in sequence_columns.clone() {
        writeln!(ddl, "CREATE SEQUENCE {};", sequence_name(column))
            .expect("writing to string is infallible");
    }

    write!(ddl, "CREATE TABLE {target} (\n    ").expect("writing to string is infallible");
    for (index, column) in columns.enumerate() {
        if index != 0 {
            ddl.push_str(",\n    ");
        }
        write!(ddl, "{} {}", quote_ident(&column.name), column.type_sql)
            .expect("writing to string is infallible");
        if let Some(default_expr) = column.default_expr.as_deref() {
            if is_regclass_nextval_default(default_expr) {
                write!(
                    ddl,
                    " DEFAULT nextval('{}'::regclass)",
                    sequence_name(column)
                )
                .expect("writing to string is infallible");
            } else {
                ddl.push_str(" DEFAULT ");
                ddl.push_str(default_expr);
            }
        }
        if column.not_null {
            ddl.push_str(" NOT NULL");
        }
    }
    ddl.push_str("\n);\n");

    for column in sequence_columns {
        writeln!(
            ddl,
            "ALTER SEQUENCE {} OWNED BY {target}.{};",
            sequence_name(column),
            quote_ident(&column.name)
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
    let mut sql = format!("COPY {} (", quoted_fq_name(target_schema, target_name));
    push_quoted_ident_list(&mut sql, effective_columns);
    write!(sql, ") FROM STDIN ({})", copy_format_options(data_format))
        .expect("writing to string is infallible");
    sql
}

const fn copy_format_options(data_format: DataFormat) -> &'static str {
    match data_format {
        DataFormat::Binary => "FORMAT binary",
        // Маркер NULL должен совпадать при экспорте и импорте.
        DataFormat::Csv => "FORMAT csv, NULL '\\\\N'",
    }
}

fn is_regclass_nextval_default(default_expr: &str) -> bool {
    let expr = default_expr.trim();
    expr.starts_with("nextval(") && expr.ends_with("::regclass)")
}

#[cfg(test)]
mod tests {
    use super::{copy_in_sql, copy_out_sql, create_table_ddl, create_view_ddl};
    use crate::pg::ColumnDef;
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
    fn rewrites_serial_default_to_target_local_sequence() {
        let ddl = create_table_ddl(
            "archive",
            "orders",
            [ColumnDef {
                name: "id".to_owned(),
                type_sql: "bigint".to_owned(),
                not_null: true,
                default_expr: Some("nextval('public.orders_id_seq'::regclass)".to_owned()),
            }]
            .iter(),
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
            [ColumnDef {
                name: "created_at".to_owned(),
                type_sql: "timestamp with time zone".to_owned(),
                not_null: true,
                default_expr: Some("now()".to_owned()),
            }]
            .iter(),
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

use anyhow::{Context, Result};

use crate::sql::quoted_fq_name;

/// После загрузки выравнивает serial/identity sequence по максимальному значению в таблице.
pub(super) async fn sync_table_sequences(
    client: &tokio_postgres::Client,
    target_schema: &str,
    target_name: &str,
    effective_columns: &[String],
) -> Result<()> {
    let sequence_columns =
        resolve_sequence_columns(client, target_schema, target_name, effective_columns).await?;
    if sequence_columns.is_empty() {
        return Ok(());
    }

    let sql = build_batch_sync_sql(target_schema, target_name, &sequence_columns);
    client.batch_execute(&sql).await.with_context(|| {
        format!("failed to synchronize serial/identity sequences for {target_schema}.{target_name}")
    })?;
    Ok(())
}

#[derive(Debug, Clone)]
struct SequenceColumn {
    column_name: String,
    sequence_name: String,
}

async fn resolve_sequence_columns(
    client: &tokio_postgres::Client,
    target_schema: &str,
    target_name: &str,
    effective_columns: &[String],
) -> Result<Vec<SequenceColumn>> {
    if effective_columns.is_empty() {
        return Ok(Vec::new());
    }

    let table_ref = quoted_fq_name(target_schema, target_name);
    let effective_columns = effective_columns.to_vec();
    let rows = client
        .query(
            "
            SELECT
                a.attname,
                pg_get_serial_sequence($1, a.attname) AS sequence_name
            FROM pg_attribute a
            JOIN pg_class c ON c.oid = a.attrelid
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = $2
              AND c.relname = $3
              AND a.attnum > 0
              AND NOT a.attisdropped
              AND a.attname = ANY($4::text[])
            ORDER BY a.attnum
            ",
            &[&table_ref, &target_schema, &target_name, &effective_columns],
        )
        .await
        .with_context(|| {
            format!(
                "failed to resolve serial/identity sequence mapping for {target_schema}.{target_name}"
            )
        })?;

    let mut resolved = Vec::new();
    for row in rows {
        let column_name: String = row.get(0);
        let sequence_name: Option<String> = row.get(1);
        if let Some(sequence_name) = sequence_name {
            resolved.push(SequenceColumn {
                column_name,
                sequence_name,
            });
        }
    }

    Ok(resolved)
}

fn build_batch_sync_sql(
    target_schema: &str,
    target_name: &str,
    sequence_columns: &[SequenceColumn],
) -> String {
    let table_ref = quoted_fq_name(target_schema, target_name);
    let escaped_table_ref = table_ref.replace('\'', "''");
    let columns_array = sql_text_array_literal(
        sequence_columns
            .iter()
            .map(|column| column.column_name.as_str()),
    );
    let sequences_array = sql_text_array_literal(
        sequence_columns
            .iter()
            .map(|column| column.sequence_name.as_str()),
    );

    // Для пустой таблицы setval выставляется на 1 с `is_called = false`,
    // чтобы следующее nextval() вернуло именно 1.
    format!(
        "DO $pgcopy$
DECLARE
    v_column text;
    v_sequence text;
    v_max bigint;
BEGIN
    FOR v_column, v_sequence IN
        SELECT *
        FROM unnest({columns_array}::text[], {sequences_array}::text[])
    LOOP
        EXECUTE format('SELECT MAX(%I)::bigint FROM {escaped_table_ref}', v_column)
            INTO v_max;
        PERFORM setval(v_sequence::regclass, COALESCE(v_max, 1), v_max IS NOT NULL);
    END LOOP;
END
$pgcopy$;"
    )
}

fn sql_text_array_literal<'a>(values: impl IntoIterator<Item = &'a str>) -> String {
    let values = values
        .into_iter()
        .map(|value| format!("'{}'", value.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    format!("ARRAY[{values}]")
}

#[cfg(test)]
mod tests {
    use super::{SequenceColumn, build_batch_sync_sql};

    #[test]
    fn builds_batch_sync_sql_for_multiple_sequence_columns() {
        let sql = build_batch_sync_sql(
            "archive",
            "orders",
            &[
                SequenceColumn {
                    column_name: "id".to_owned(),
                    sequence_name: "archive.orders_id_seq".to_owned(),
                },
                SequenceColumn {
                    column_name: "external_id".to_owned(),
                    sequence_name: "archive.orders_external_id_seq".to_owned(),
                },
            ],
        );

        assert!(sql.contains("DO $pgcopy$"));
        assert!(sql.contains("FROM unnest("));
        assert!(sql.contains("'id'"));
        assert!(sql.contains("'external_id'"));
        assert!(sql.contains("'archive.orders_id_seq'"));
        assert!(sql.contains("'archive.orders_external_id_seq'"));
        assert!(
            sql.contains("EXECUTE format('SELECT MAX(%I)::bigint FROM \"archive\".\"orders\"'")
        );
        assert!(sql.contains("PERFORM setval(v_sequence::regclass"));
    }

    #[test]
    fn escapes_single_quotes_in_sequence_name() {
        let sql = build_batch_sync_sql(
            "archive",
            "orders",
            &[SequenceColumn {
                column_name: "id".to_owned(),
                sequence_name: "archive.o'hare_seq".to_owned(),
            }],
        );

        assert!(sql.contains("'archive.o''hare_seq'"));
    }

    #[test]
    fn escapes_single_quotes_in_column_name() {
        let sql = build_batch_sync_sql(
            "archive",
            "orders",
            &[SequenceColumn {
                column_name: "o'hare_id".to_owned(),
                sequence_name: "archive.orders_id_seq".to_owned(),
            }],
        );

        assert!(sql.contains("'o''hare_id'"));
    }
}

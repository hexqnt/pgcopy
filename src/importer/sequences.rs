use anyhow::{Context, Result};

use crate::sql::{quote_ident, quoted_fq_name};

/// После загрузки выравнивает serial/identity sequence по максимальному значению в таблице.
pub(super) async fn sync_table_sequences(
    client: &tokio_postgres::Client,
    target_schema: &str,
    target_name: &str,
    effective_columns: &[String],
) -> Result<()> {
    for column_name in effective_columns {
        let sequence_name =
            sequence_name_for_column(client, target_schema, target_name, column_name).await?;
        if let Some(sequence_name) = sequence_name {
            sync_sequence_to_max(
                client,
                target_schema,
                target_name,
                column_name,
                &sequence_name,
            )
            .await?;
        }
    }
    Ok(())
}

async fn sequence_name_for_column(
    client: &tokio_postgres::Client,
    target_schema: &str,
    target_name: &str,
    column_name: &str,
) -> Result<Option<String>> {
    let table_ref = quoted_fq_name(target_schema, target_name);
    let row = client
        .query_one(
            "SELECT pg_get_serial_sequence($1, $2)",
            &[&table_ref, &column_name],
        )
        .await
        .with_context(|| {
            format!(
                "failed to resolve serial/identity sequence for {}.{} column '{}'",
                target_schema, target_name, column_name
            )
        })?;
    Ok(row.get::<_, Option<String>>(0))
}

async fn sync_sequence_to_max(
    client: &tokio_postgres::Client,
    target_schema: &str,
    target_name: &str,
    column_name: &str,
    sequence_name: &str,
) -> Result<()> {
    let quoted_table = quoted_fq_name(target_schema, target_name);
    let quoted_column = quote_ident(column_name);
    // Для пустой таблицы setval выставляется на 1 с `is_called = false`,
    // чтобы следующее nextval() вернуло именно 1.
    let sql = format!(
        "SELECT setval($1::regclass, COALESCE(v.max_value, 1), v.max_value IS NOT NULL) \
         FROM (SELECT MAX({quoted_column})::bigint AS max_value FROM {quoted_table}) AS v"
    );
    client
        .query_one(&sql, &[&sequence_name])
        .await
        .with_context(|| {
            format!(
                "failed to synchronize sequence '{}' with {}.{} column '{}'",
                sequence_name, target_schema, target_name, column_name
            )
        })?;
    Ok(())
}

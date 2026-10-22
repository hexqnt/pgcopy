use anyhow::{Context, Result};

/// Запускает экспортную операцию с опциональной consistent-snapshot транзакцией.
///
/// Если `export_snapshot_for_workers = true`, дополнительно вызывает
/// `pg_export_snapshot()` и передаёт id в `operation`.
pub async fn run_with_snapshot_support<T, Op, Fut>(
    client: &tokio_postgres::Client,
    consistent_snapshot: bool,
    export_snapshot_for_workers: bool,
    operation: Op,
) -> Result<T>
where
    Op: FnOnce(Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    if !consistent_snapshot {
        return operation(None).await;
    }

    execute_snapshot_transaction_statement(
        client,
        "BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY",
        "begin",
    )
    .await?;
    let snapshot_id = if export_snapshot_for_workers {
        Some(export_snapshot_id(client).await?)
    } else {
        None
    };

    match operation(snapshot_id).await {
        Ok(value) => {
            execute_snapshot_transaction_statement(client, "COMMIT", "commit").await?;
            Ok(value)
        }
        Err(error) => {
            let _ = execute_snapshot_transaction_statement(client, "ROLLBACK", "rollback").await;
            Err(error)
        }
    }
}

async fn export_snapshot_id(client: &tokio_postgres::Client) -> Result<String> {
    let snapshot_row = client
        .query_one("SELECT pg_export_snapshot()", &[])
        .await
        .context("failed to export PostgreSQL snapshot for parallel export")?;
    Ok(snapshot_row.get(0))
}

async fn execute_snapshot_transaction_statement(
    client: &tokio_postgres::Client,
    sql: &str,
    action: &str,
) -> Result<()> {
    client
        .batch_execute(sql)
        .await
        .with_context(|| format!("failed to {action} consistent export snapshot transaction"))
}

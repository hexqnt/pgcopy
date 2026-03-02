use anyhow::{Context, Result};

use crate::manifest::ManifestObject;

/// Выполняет replace-операцию в явной транзакции `BEGIN/COMMIT/ROLLBACK`.
///
/// При ошибке rollback тоже проверяется и возвращается в цепочке контекста.
pub(super) async fn run_replace_atomically<T, F>(
    client: &tokio_postgres::Client,
    object: &ManifestObject,
    operation: F,
) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    begin_replace_transaction(client, object).await?;

    match operation.await {
        Ok(value) => {
            commit_replace_transaction(client, object).await?;
            Ok(value)
        }
        Err(error) => {
            if let Err(rollback_error) = rollback_replace_transaction(client, object).await {
                return Err(error.context(format!(
                    "failed to rollback replace transaction for {}.{}: {rollback_error}",
                    object.target_schema, object.target_name
                )));
            }
            Err(error)
        }
    }
}

async fn begin_replace_transaction(
    client: &tokio_postgres::Client,
    object: &ManifestObject,
) -> Result<()> {
    execute_replace_transaction_statement(client, object, "BEGIN", "begin").await
}

async fn commit_replace_transaction(
    client: &tokio_postgres::Client,
    object: &ManifestObject,
) -> Result<()> {
    execute_replace_transaction_statement(client, object, "COMMIT", "commit").await
}

async fn rollback_replace_transaction(
    client: &tokio_postgres::Client,
    object: &ManifestObject,
) -> Result<()> {
    execute_replace_transaction_statement(client, object, "ROLLBACK", "rollback").await
}

async fn execute_replace_transaction_statement(
    client: &tokio_postgres::Client,
    object: &ManifestObject,
    sql: &str,
    action: &str,
) -> Result<()> {
    client.batch_execute(sql).await.with_context(|| {
        format!(
            "failed to {action} replace transaction for {}.{}",
            object.target_schema, object.target_name
        )
    })
}

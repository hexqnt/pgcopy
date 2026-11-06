use std::path::Path;

use anyhow::{Context, Result};
use tempfile::TempDir;
use tokio_postgres::{Client, NoTls};

use crate::config::ObjectConfig;
use crate::manifest::ManifestObject;
use crate::retry::RetryPolicy;
use crate::types::DataFormat;

use super::object::export_object;

#[derive(Clone, Copy)]
pub(super) struct ExportContext<'a> {
    pub source_config: &'a tokio_postgres::Config,
    pub scratch_dir: &'a Path,
    pub data_format: DataFormat,
    pub snapshot_id: Option<&'a str>,
    pub retry: RetryPolicy,
}

/// Прерывает драйвер при отмене попытки, закрывая сокет даже во время COPY.
struct ExportConnection {
    client: Client,
    driver: tokio::task::JoinHandle<()>,
}

impl Drop for ExportConnection {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

/// Успешные объекты переиспользуют соединение; ошибка или таймаут уничтожает его.
pub(super) struct ExportSession<'a> {
    context: ExportContext<'a>,
    connection: Option<ExportConnection>,
}

impl<'a> ExportSession<'a> {
    pub(super) const fn new(context: ExportContext<'a>) -> Self {
        Self {
            context,
            connection: None,
        }
    }

    pub(super) async fn export(
        &mut self,
        index: usize,
        object: &ObjectConfig,
    ) -> Result<ManifestObject> {
        let context = self.context;
        let (manifest, attempt_dir, connection) = self
            .context
            .retry
            .run(&object.source_label(), || {
                Self::export_attempt(context, self.connection.take(), index, object)
            })
            .await?;
        self.connection = Some(connection);

        // Публикуем только полностью записанные файлы, за пределами отменяемой попытки.
        for path in [&manifest.ddl_path, &manifest.data_path] {
            std::fs::rename(
                attempt_dir.path().join(path),
                self.context.scratch_dir.join(path),
            )
            .with_context(|| format!("failed to publish export file {path}"))?;
        }
        Ok(manifest)
    }

    async fn export_attempt(
        context: ExportContext<'_>,
        connection: Option<ExportConnection>,
        index: usize,
        object: &ObjectConfig,
    ) -> Result<(ManifestObject, TempDir, ExportConnection)> {
        // Отдельный каталог исключает гонку с незавершённой файловой операцией Tokio
        // после отмены предыдущей попытки: новые файлы всегда имеют другие inode.
        let attempt_dir = tempfile::tempdir_in(context.scratch_dir)
            .context("failed to create temporary directory for export attempt")?;
        let connection = if let Some(connection) = connection {
            connection
        } else {
            let (client, connection) = context
                .source_config
                .connect(NoTls)
                .await
                .context("failed to connect source database for export attempt")?;
            let driver = tokio::spawn(async move {
                if let Err(error) = connection.await {
                    tracing::warn!(%error, "export connection task failed");
                }
            });
            ExportConnection { client, driver }
        };
        let client = &connection.client;
        if let Some(snapshot_id) = context.snapshot_id {
            client
                .batch_execute("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .await
                .context("failed to begin export snapshot transaction")?;
            set_transaction_snapshot(client, snapshot_id).await?;
        }
        let manifest = export_object(
            client,
            attempt_dir.path(),
            index,
            object,
            context.data_format,
        )
        .await?;
        if context.snapshot_id.is_some() {
            client
                .batch_execute("COMMIT")
                .await
                .context("failed to commit export snapshot transaction")?;
        }
        Ok((manifest, attempt_dir, connection))
    }
}

async fn set_transaction_snapshot(client: &Client, snapshot_id: &str) -> Result<()> {
    let escaped_snapshot = snapshot_id.replace('\'', "''");
    client
        .batch_execute(&format!("SET TRANSACTION SNAPSHOT '{escaped_snapshot}'"))
        .await
        .context("failed to set transaction snapshot")
}

/// Запускает экспортную операцию с опциональной consistent-snapshot транзакцией.
/// Снимок экспортируется и для одного worker-а, чтобы переподключение сохраняло его.
pub async fn run_with_snapshot_support<T, Op, Fut>(
    client: &tokio_postgres::Client,
    consistent_snapshot: bool,
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
    let snapshot_id = Some(export_snapshot_id(client).await?);

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
        .context("failed to export PostgreSQL snapshot")?;
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

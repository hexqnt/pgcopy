use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use bytes::Bytes;
use futures_util::{SinkExt, pin_mut};
use tokio::io::AsyncReadExt;

use crate::pg;
use crate::types::DataFormat;

/// Загружает данные в target через `COPY FROM STDIN` из файла на диске.
pub async fn copy_data_in_file(
    client: &tokio_postgres::Client,
    data_path: &Path,
    target_schema: &str,
    target_name: &str,
    effective_columns: &[String],
    data_format: DataFormat,
) -> Result<()> {
    let copy_sql = pg::copy_in_sql(target_schema, target_name, effective_columns, data_format);
    let copy_sink = client
        .copy_in(&copy_sql)
        .await
        .with_context(|| format!("failed to start COPY IN into {target_schema}.{target_name}"))?;
    pin_mut!(copy_sink);

    let mut file = tokio::fs::File::open(data_path)
        .await
        .with_context(|| format!("failed to open data file {}", data_path.display()))?;

    // 64 KiB — компромисс между числом syscalls и памятью на поток.
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .with_context(|| format!("failed to read data file {}", data_path.display()))?;

        if read == 0 {
            break;
        }

        copy_sink
            .as_mut()
            .send(Bytes::copy_from_slice(&buffer[..read]))
            .await
            .context("failed to stream chunk to COPY IN")?;
    }

    copy_sink
        .as_mut()
        .finish()
        .await
        .context("failed to finish COPY IN")?;

    Ok(())
}

/// Загружает данные в target через `COPY FROM STDIN` из произвольного `Read`.
pub async fn copy_data_in_reader<R: Read>(
    client: &tokio_postgres::Client,
    reader: &mut R,
    target_schema: &str,
    target_name: &str,
    effective_columns: &[String],
    data_format: DataFormat,
) -> Result<()> {
    let copy_sql = pg::copy_in_sql(target_schema, target_name, effective_columns, data_format);
    let copy_sink = client
        .copy_in(&copy_sql)
        .await
        .with_context(|| format!("failed to start COPY IN into {target_schema}.{target_name}"))?;
    pin_mut!(copy_sink);

    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .context("failed to read streaming data from bundle")?;

        if read == 0 {
            break;
        }

        copy_sink
            .as_mut()
            .send(Bytes::copy_from_slice(&buffer[..read]))
            .await
            .context("failed to stream chunk to COPY IN")?;
    }

    copy_sink
        .as_mut()
        .finish()
        .await
        .context("failed to finish COPY IN")?;

    Ok(())
}

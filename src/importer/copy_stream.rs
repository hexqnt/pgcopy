use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use bytes::Bytes;
use futures_util::{SinkExt, pin_mut};
use tokio::io::AsyncReadExt;

use crate::pg;
use crate::types::DataFormat;

const COPY_CHUNK_SIZE: usize = 64 * 1024;

pub(super) trait CopyChunkSource {
    async fn read_chunk(&mut self, buffer: &mut [u8]) -> Result<usize>;
}

pub(super) struct FileChunkSource<'a> {
    file: tokio::fs::File,
    data_path: &'a Path,
}

impl<'a> FileChunkSource<'a> {
    pub(super) async fn open(data_path: &'a Path) -> Result<Self> {
        let file = tokio::fs::File::open(data_path)
            .await
            .with_context(|| format!("failed to open data file {}", data_path.display()))?;

        Ok(Self { file, data_path })
    }
}

impl CopyChunkSource for FileChunkSource<'_> {
    async fn read_chunk(&mut self, buffer: &mut [u8]) -> Result<usize> {
        self.file
            .read(buffer)
            .await
            .with_context(|| format!("failed to read data file {}", self.data_path.display()))
    }
}

pub(super) struct ReaderChunkSource<'a, R: Read> {
    reader: &'a mut R,
}

impl<'a, R: Read> ReaderChunkSource<'a, R> {
    pub(super) fn new(reader: &'a mut R) -> Self {
        Self { reader }
    }
}

impl<R: Read> CopyChunkSource for ReaderChunkSource<'_, R> {
    async fn read_chunk(&mut self, buffer: &mut [u8]) -> Result<usize> {
        tokio::task::block_in_place(|| self.reader.read(buffer))
            .context("failed to read streaming data from bundle")
    }
}

pub(super) async fn copy_data_in<S: CopyChunkSource>(
    client: &tokio_postgres::Client,
    source: &mut S,
    target_schema: &str,
    target_name: &str,
    effective_columns: &[String],
    data_format: DataFormat,
) -> Result<u64> {
    let copy_sql = pg::copy_in_sql(target_schema, target_name, effective_columns, data_format);
    let copy_sink = client
        .copy_in(&copy_sql)
        .await
        .with_context(|| format!("failed to start COPY IN into {target_schema}.{target_name}"))?;
    pin_mut!(copy_sink);

    // 64 KiB — компромисс между числом syscalls и памятью на поток.
    let mut buffer = vec![0_u8; COPY_CHUNK_SIZE];
    loop {
        let read = source.read_chunk(&mut buffer).await?;

        if read == 0 {
            break;
        }

        copy_sink
            .as_mut()
            .send(Bytes::copy_from_slice(&buffer[..read]))
            .await
            .context("failed to stream chunk to COPY IN")?;
    }

    let inserted_rows = copy_sink
        .as_mut()
        .finish()
        .await
        .context("failed to finish COPY IN")?;

    Ok(inserted_rows)
}

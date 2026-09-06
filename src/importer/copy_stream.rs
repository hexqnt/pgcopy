use std::{collections::VecDeque, io::Read, path::Path};

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use futures_util::{SinkExt, pin_mut};
use tokio::io::AsyncReadExt;

use crate::pg;
use crate::types::DataFormat;

// Размер полного блока zstd: декодер способен заполнить такой буфер за один вызов.
const COPY_CHUNK_SIZE: usize = 128 * 1024;
// Глубина bounded-канала COPY IN в tokio-postgres с учётом слота sender-а.
const COPY_PIPELINE_DEPTH: usize = 2;

struct CopyBufferPool {
    pending: VecDeque<Bytes>,
}

impl CopyBufferPool {
    fn new() -> Self {
        Self {
            pending: VecDeque::with_capacity(COPY_PIPELINE_DEPTH),
        }
    }

    fn acquire(&mut self) -> BytesMut {
        let mut buffer = self
            .try_reclaim_oldest()
            .unwrap_or_else(|| BytesMut::zeroed(COPY_CHUNK_SIZE));
        buffer.resize(COPY_CHUNK_SIZE, 0);
        buffer
    }

    fn try_reclaim_oldest(&mut self) -> Option<BytesMut> {
        if self.pending.len() < COPY_PIPELINE_DEPTH {
            return None;
        }

        // После прохождения backpressure tokio-postgres обычно уже освободил
        // свою ссылку. Если нет, корректный fallback — выделить новый буфер.
        self.pending.pop_front()?.try_into_mut().ok()
    }

    fn retain(&mut self, chunk: Bytes) {
        debug_assert!(self.pending.len() < COPY_PIPELINE_DEPTH);
        self.pending.push_back(chunk);
    }
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
    pub(super) const fn new(reader: &'a mut R) -> Self {
        Self { reader }
    }
}

impl<R: Read> CopyChunkSource for ReaderChunkSource<'_, R> {
    async fn read_chunk(&mut self, buffer: &mut [u8]) -> Result<usize> {
        tokio::task::block_in_place(|| self.reader.read(buffer))
            .context("failed to read streaming data from bundle")
    }
}

pub(super) trait CopyChunkSource {
    async fn read_chunk(&mut self, buffer: &mut [u8]) -> Result<usize>;
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

    let mut buffers = CopyBufferPool::new();
    loop {
        let mut buffer = buffers.acquire();
        let read = source.read_chunk(&mut buffer).await?;

        if read == 0 {
            break;
        }

        buffer.truncate(read);
        let chunk = buffer.freeze();
        copy_sink
            .as_mut()
            .send(chunk.clone())
            .await
            .context("failed to stream chunk to COPY IN")?;
        buffers.retain(chunk);
    }

    let inserted_rows = copy_sink
        .as_mut()
        .finish()
        .await
        .context("failed to finish COPY IN")?;

    Ok(inserted_rows)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::{COPY_CHUNK_SIZE, CopyBufferPool};

    fn submit_chunk(pool: &mut CopyBufferPool) -> (Bytes, *const u8) {
        let mut buffer = pool.acquire();
        let buffer_ptr = buffer.as_ptr();
        buffer.truncate(1);

        let sent = buffer.freeze();
        pool.retain(sent.clone());
        (sent, buffer_ptr)
    }

    #[test]
    fn reuses_oldest_released_buffer() {
        let mut pool = CopyBufferPool::new();
        let (oldest, oldest_ptr) = submit_chunk(&mut pool);
        drop(oldest);
        let (newest, _) = submit_chunk(&mut pool);
        drop(newest);

        let buffer = pool.acquire();
        assert_eq!(buffer.as_ptr(), oldest_ptr);
        assert_eq!(buffer.len(), COPY_CHUNK_SIZE);
    }

    #[test]
    fn allocates_fallback_while_oldest_buffer_is_retained() {
        let mut pool = CopyBufferPool::new();
        let (retained, retained_ptr) = submit_chunk(&mut pool);
        let (newest, _) = submit_chunk(&mut pool);
        drop(newest);

        let buffer = pool.acquire();
        assert_ne!(buffer.as_ptr(), retained_ptr);
        assert_eq!(buffer.len(), COPY_CHUNK_SIZE);
        drop(retained);
    }
}

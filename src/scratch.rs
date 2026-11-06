use std::borrow::Cow;
use std::fmt;
use std::path::Path;

use clap::ValueEnum;
use serde::Deserialize;

/// Сжатие промежуточных COPY-файлов; формат bundle от него не зависит.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TempCompression {
    None,
    #[default]
    Zstd,
}

impl TempCompression {
    pub(crate) fn path(self, logical_path: &Path) -> Cow<'_, Path> {
        match self {
            Self::None => Cow::Borrowed(logical_path),
            Self::Zstd => Cow::Owned(logical_path.with_added_extension("zst")),
        }
    }
}

impl fmt::Display for TempCompression {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::None => "none",
            Self::Zstd => "zstd",
        })
    }
}

/// Размер исходных данных нужен tar-заголовку до начала потоковой распаковки.
#[derive(Clone, Copy)]
pub(crate) enum ScratchData {
    Plain,
    Zstd { uncompressed_size: u64 },
}

impl ScratchData {
    pub(crate) fn path(self, logical_path: &Path) -> Cow<'_, Path> {
        let compression = match self {
            Self::Plain => TempCompression::None,
            Self::Zstd { .. } => TempCompression::Zstd,
        };
        compression.path(logical_path)
    }
}

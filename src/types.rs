use serde::{Deserialize, Serialize};
use std::fmt;

/// Формат COPY-данных, в котором объект хранится внутри bundle.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DataFormat {
    Binary,
    Csv,
}

impl DataFormat {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::Csv => "csv",
        }
    }
}

impl fmt::Display for DataFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

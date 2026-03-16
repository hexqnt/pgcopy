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

/// Режим экспорта объекта: материализовать в таблицу или сохранить как view.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ExportAs {
    #[default]
    Table,
    View,
}

impl ExportAs {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::View => "view",
        }
    }

    pub const fn requires_data_payload(self) -> bool {
        matches!(self, Self::Table)
    }
}

impl fmt::Display for ExportAs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

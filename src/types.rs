use serde::{Deserialize, Serialize};

/// Формат COPY-данных, в котором объект хранится внутри bundle.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DataFormat {
    Binary,
    Csv,
}

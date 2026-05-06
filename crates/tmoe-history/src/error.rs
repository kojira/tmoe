use thiserror::Error;

#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid data: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, HistoryError>;

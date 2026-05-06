use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("invalid URL: {0}")]
    Url(#[from] url::ParseError),

    #[error("backend returned status {status}: {body}")]
    BadStatus { status: u16, body: String },

    #[error("response decode error: {0}")]
    Decode(String),

    #[error("scripted mock exhausted")]
    MockExhausted,

    #[error("other: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, LlmError>;

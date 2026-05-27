use thiserror::Error;

#[derive(Debug, Error)]
pub enum SegmentError {
    #[error("max_tokens must be positive")]
    InvalidMaxTokens,

    #[error("max_tokens is too small for the tokenizer output")]
    MaxTokensTooSmall,

    #[error("internal segmentation error: unit exceeds max_tokens")]
    UnitExceedsMaxTokens,

    #[error("tokenizer error: {0}")]
    Tokenizer(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("tokenizer download error: {0}")]
    Download(String),
}

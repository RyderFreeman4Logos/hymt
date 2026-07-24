use thiserror::Error;

#[derive(Debug, Error)]
pub enum SegmentError {
    #[error("max_tokens must be positive")]
    InvalidMaxTokens,

    #[error("max_tokens is too small for the tokenizer output")]
    MaxTokensTooSmall,

    #[error(
        "ProtectedBlockTooLarge: protected block too large: {tokens} tokens exceeds segment limit {max_tokens}; split it or preserve it outside the model"
    )]
    ProtectedBlockTooLarge { tokens: usize, max_tokens: usize },

    #[error("internal segmentation error: unit exceeds max_tokens")]
    UnitExceedsMaxTokens,

    #[error("tokenizer error: {0}")]
    Tokenizer(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("tokenizer download error: {0}")]
    Download(String),
}

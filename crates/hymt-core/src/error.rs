use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("config parse error: {0}")]
    Config(String),

    #[error("unsupported language '{0}'")]
    UnsupportedLanguage(String),

    #[error("unsupported template type '{0}'")]
    InvalidTemplate(String),

    #[error("missing required option '{0}' for this template type")]
    MissingTemplateOption(String),
}

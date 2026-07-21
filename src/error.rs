use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("LAVIS_API_ID is not set")]
    MissingApiId,
    #[error("LAVIS_API_ID must be a positive integer")]
    InvalidApiId,
    #[error("LAVIS_API_HASH is not set")]
    MissingApiHash,
    #[error("LAVIS_API_HASH must be a non-empty Unicode value")]
    InvalidApiHash,
    #[error("command prefix must not be empty")]
    EmptyPrefix,
    #[error("session path must not be empty")]
    EmptySessionPath,
    #[error("neither XDG_STATE_HOME nor HOME is available for the session path")]
    MissingStateDirectory,
}

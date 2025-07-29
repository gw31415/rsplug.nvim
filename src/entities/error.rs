use std::io;

/// メインのエラー型
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error(transparent)]
    Serde(#[from] toml::de::Error),
    #[error(transparent)]
    Regex(#[from] regex::Error),
    #[error(transparent)]
    ExternalSystem(#[from] ExternalSystemError),
}

/// システム由来のエラー型
#[derive(thiserror::Error, Debug)]
pub enum ExternalSystemError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("Process failed: {}", String::from_utf8_lossy(stderr))]
    ProcessFailed { stderr: Vec<u8> },
    #[error(transparent)]
    Utf8(#[from] std::string::FromUtf8Error),
}

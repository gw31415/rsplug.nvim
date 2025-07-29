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
    ExternalCommand(#[from] ExternalCommandError),
}

/// メインの結果型
pub type MainResult<T = ()> = Result<T, Error>;

/// 外部コマンドのエラー型
#[derive(thiserror::Error, Debug)]
pub enum ExternalCommandError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("Execution failed: {}", String::from_utf8_lossy(stderr))]
    Failed { stderr: Vec<u8> },
}

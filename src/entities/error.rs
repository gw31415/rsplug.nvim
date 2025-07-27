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
}

/// メインの結果型
pub type MainResult<T = ()> = Result<T, Error>;

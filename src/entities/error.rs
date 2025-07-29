use std::io;

/// System-derived errors which cannot be handled by the application.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// IO Error
    #[error(transparent)]
    Io(#[from] io::Error),
    /// External process failed with non-zero exit code
    #[error("Process failed: {}", String::from_utf8_lossy(stderr))]
    ProcessFailed { stderr: Vec<u8> },
    /// If paths or outputs are not valid UTF-8
    #[error(transparent)]
    Utf8(#[from] std::string::FromUtf8Error),
}

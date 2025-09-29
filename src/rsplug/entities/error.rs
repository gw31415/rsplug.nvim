use std::{io, sync::Arc};

/// System-derived errors which cannot be handled by the application.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// IO Error
    #[error(transparent)]
    Io(#[from] io::Error),
    /// External process failed with non-zero exit code
    #[error("Process failed: {}", String::from_utf8_lossy(stderr))]
    ProcessFailed { stderr: Vec<u8> },
    /// Git Revision Error
    #[error("Git-rev of {:?} not found in {url}", &rev)]
    GitRev { url: Arc<str>, rev: String },
    /// If paths or outputs are not valid UTF-8
    #[error(transparent)]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error(transparent)]
    Git2(#[from] git2::Error),
}

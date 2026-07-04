use std::{env::current_dir, io, io::Write, path::PathBuf};
use tokio::io::AsyncReadExt;
use tokio::{sync::mpsc, task::JoinHandle};
use walker::{
    compiled_glob::CompiledGlob,
    walker::{EntryKind, WalkError, Walker, WalkerOptions},
};

pub struct ConfigWalker {
    rx: mpsc::UnboundedReceiver<Result<PathBuf, io::Error>>,
    _handle: JoinHandle<()>,
    /// Materialized stdin content for the "-" argument, kept alive for the
    /// walker's lifetime so downstream path-based readers can read it; it is
    /// unlinked when the walker is dropped.
    _stdin_temp: Option<tempfile::NamedTempFile>,
}

/// Reads standard input to end into a buffer.
async fn read_stdin_to_end() -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    tokio::io::stdin().read_to_end(&mut buf).await?;
    Ok(buf)
}

/// Writes the given bytes into a fresh temporary file and returns it. The
/// caller owns the file; dropping it unlinks it from the filesystem.
fn materialize_stdin(bytes: &[u8]) -> io::Result<tempfile::NamedTempFile> {
    let mut temp = tempfile::NamedTempFile::new()?;
    temp.write_all(bytes)?;
    temp.flush()?;
    Ok(temp)
}

fn is_ignorable_walk_error(e: &io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        e.kind(),
        ErrorKind::NotFound | ErrorKind::NotADirectory | ErrorKind::PermissionDenied
    )
}

impl ConfigWalker {
    pub fn recv(&mut self) -> impl Future<Output = Option<Result<PathBuf, io::Error>>> {
        self.rx.recv()
    }

    pub async fn new(patterns: Vec<String>) -> Result<ConfigWalker, io::Error> {
        let mut direct_files = Vec::new();
        let mut compiled_patterns = Vec::with_capacity(patterns.len());
        let mut stdin_temp: Option<tempfile::NamedTempFile> = None;
        for pattern in patterns {
            if pattern == "-" {
                // A lone "-" conventionally means standard input. Materialize
                // the piped/redirected TOML into a temp file and feed it through
                // the same path-based pipeline as ordinary config files. stdin
                // can only be read once, so repeated "-" reuse the same file.
                let path = match &stdin_temp {
                    Some(existing) => existing.path().to_path_buf(),
                    None => {
                        let bytes = read_stdin_to_end().await?;
                        let temp = materialize_stdin(&bytes)?;
                        let path = temp.path().to_path_buf();
                        stdin_temp = Some(temp);
                        path
                    }
                };
                direct_files.push(path);
                continue;
            }
            let path = PathBuf::from(&pattern);
            if path.is_file() {
                direct_files.push(path);
            } else {
                compiled_patterns.push(CompiledGlob::new(&pattern)?);
            }
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let _cwd = current_dir()?;
        let options = WalkerOptions {
            files_only: true,
            ..WalkerOptions::default()
        };
        let handle = tokio::spawn(async move {
            for path in direct_files {
                let _ = tx.send(Ok(path));
            }

            if compiled_patterns.is_empty() {
                return;
            }

            let mut walker = Walker::spawn_many_with_options(compiled_patterns, options);
            while let Some(item) = walker.recv().await {
                match item {
                    Ok(event) => {
                        if event.kind == EntryKind::File {
                            let _ = tx.send(Ok(event.path));
                        }
                    }
                    Err(WalkError::Io { source, .. }) => {
                        if is_ignorable_walk_error(&source) {
                            continue;
                        }
                        let _ = tx.send(Err(source));
                    }
                    Err(err) => {
                        let _ = tx.send(Err(io::Error::other(err.to_string())));
                    }
                }
            }
        });
        Ok(ConfigWalker {
            rx,
            _handle: handle,
            _stdin_temp: stdin_temp,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialize_stdin_roundtrips_content() {
        let content = "[[plugins]]\nrepo = \"owner/repo\"\n";
        let temp = materialize_stdin(content.as_bytes()).unwrap();
        let read_back = std::fs::read(temp.path()).unwrap();
        assert_eq!(read_back, content.as_bytes());
    }

    #[test]
    fn materialize_stdin_persists_until_dropped() {
        let temp = materialize_stdin(b"hello").unwrap();
        let path = temp.path().to_path_buf();
        assert!(path.exists());
        drop(temp);
        assert!(!path.exists());
    }
}

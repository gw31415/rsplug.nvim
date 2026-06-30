use std::{env::current_dir, io, path::PathBuf};
use tokio::{sync::mpsc, task::JoinHandle};
use walker::{
    compiled_glob::CompiledGlob,
    walker::{EntryKind, WalkError, Walker, WalkerOptions},
};

pub struct ConfigWalker {
    rx: mpsc::UnboundedReceiver<Result<PathBuf, io::Error>>,
    _handle: JoinHandle<()>,
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
        for pattern in patterns {
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
        })
    }
}

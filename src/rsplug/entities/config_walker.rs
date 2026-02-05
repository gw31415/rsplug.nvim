use std::{env::current_dir, io, path::PathBuf};
use tokio::{sync::mpsc, task::JoinHandle};

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

    pub fn new(patterns: Vec<String>) -> Result<ConfigWalker, io::Error> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut walker = globwalker::GlobWalker::new(patterns, &current_dir()?)?;
        let handle = tokio::spawn(async move {
            loop {
                let _ = tx.send(match walker.next().await {
                    Ok(None) => {
                        break;
                    }
                    Ok(Some(path)) => Ok(path.into()),
                    Err(err) => {
                        if is_ignorable_walk_error(&err) {
                            continue;
                        }
                        Err(err)
                    }
                });
            }
        });
        Ok(ConfigWalker {
            rx,
            _handle: handle,
        })
    }
}

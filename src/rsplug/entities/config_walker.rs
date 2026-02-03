use std::{path::Path, sync::Arc};
use tokio::{sync::mpsc, task::JoinHandle};

use super::util;

pub struct ConfigWalker {
    rx: mpsc::UnboundedReceiver<Result<Arc<Path>, ignore::Error>>,
    _handle: JoinHandle<()>,
}

impl ConfigWalker {
    pub fn recv(&mut self) -> impl Future<Output = Option<Result<Arc<Path>, ignore::Error>>> {
        self.rx.recv()
    }

    pub fn new(patterns: Vec<String>) -> ConfigWalker {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = tokio::task::spawn_blocking(move || {
            let iter = match util::glob::find(patterns.iter().map(String::as_str)) {
                Ok(iter) => iter,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            };
            for entry in iter {
                match entry {
                    Ok(path) => {
                        if path.is_dir() {
                            continue;
                        }
                        let path: Arc<Path> = path.into();
                        let _ = tx.send(Ok(path));
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            }
        });
        ConfigWalker {
            rx,
            _handle: handle,
        }
    }
}

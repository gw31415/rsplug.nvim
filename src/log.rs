use colored::Colorize;
use once_cell::sync::Lazy;
use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
};
use tokio::sync::{Mutex, mpsc};

pub enum Message {
    DetectConfigFile(PathBuf),
    CheckingLocalPlugins { install: bool, update: bool },
    TotalPackages(usize),
    TotalPackagesMerged(usize),
    Cache(&'static str, Arc<str>),
    InstallSkipped(Arc<str>),
    InstallYank { id: Arc<str>, which: PathBuf },
    Error(Box<dyn std::error::Error + 'static + Send + Sync>),
}

type MessageSender = RwLock<Option<mpsc::UnboundedSender<Message>>>;
type LoggerCloser = Mutex<mpsc::UnboundedReceiver<()>>;
type Logger = (MessageSender, LoggerCloser);

static LOGGER: Lazy<Logger> = Lazy::new(init);

fn init() -> Logger {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (tx_end, rx_end) = mpsc::unbounded_channel::<()>();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                Message::DetectConfigFile(path) => {
                    println!(
                        "{} Using config file: {}",
                        "info:".blue().bold(),
                        path.to_string_lossy().italic().dimmed()
                    );
                }
                Message::CheckingLocalPlugins { install, update } => {
                    let activity = if install && update {
                        "Checking installed plugins & updates"
                    } else if update {
                        "Checking updates"
                    } else if install {
                        "Checking installed plugins"
                    } else {
                        "Loading local plugins"
                    };
                    println!("{} {}", "info:".blue().bold(), activity);
                }
                Message::TotalPackages(n) => {
                    println!("{} Raw packages count {n}", "info:".blue().bold());
                }
                Message::TotalPackagesMerged(n) => {
                    println!("{} Merged packages count {n}", "info:".blue().bold());
                }
                Message::Cache(r#type, url) => {
                    println!("{} {type} {url}", "info:".blue().bold());
                }
                Message::InstallSkipped(id) => {
                    println!("{} Skipped {}", "info:".blue().bold(), id.italic().dimmed(),);
                }
                Message::InstallYank { id, which: file } => {
                    println!(
                        "{} Copying in {}: {}",
                        "info:".blue().bold(),
                        id.italic().dimmed(),
                        file.to_string_lossy()
                    );
                }
                Message::Error(e) => {
                    println!("{} {}", "error:".red().bold(), e);
                }
            }
        }
        let _ = tx_end.send(());
    });
    (Some(tx).into(), rx_end.into())
}

/// Output log messages
pub fn msg(message: Message) {
    let _ = LOGGER.0.read().unwrap().as_ref().unwrap().send(message);
}

/// Flush out the rest of the log and exit
pub async fn close(code: i32) -> ! {
    loop {
        if let Ok(mut sender) = LOGGER.0.try_write() {
            let sender: &mut Option<_> = &mut sender;
            drop(std::mem::take(sender));
        } else {
            continue;
        }
        LOGGER.1.lock().await.recv().await;
        std::process::exit(code);
    }
}

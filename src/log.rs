use colored::Colorize;
use hashbrown::HashMap;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use once_cell::sync::Lazy;
use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::{Mutex, mpsc};

pub enum Message {
    DetectConfigFile(PathBuf),
    CheckingLocalPlugins { install: bool, update: bool },
    CheckingLocalPluginsFinished { total: usize, merged: usize },
    Cache(&'static str, Arc<str>),
    CacheDone,
    InstallSkipped(Arc<str>),
    InstallYank { id: Arc<str>, which: PathBuf },
    InstallDone,
    Error(Box<dyn std::error::Error + 'static + Send + Sync>),
}

type MessageSender = RwLock<Option<mpsc::UnboundedSender<Message>>>;
type LoggerCloser = Mutex<mpsc::UnboundedReceiver<()>>;
type Logger = (MessageSender, LoggerCloser);

static LOGGER: Lazy<Logger> = Lazy::new(init);

fn init() -> Logger {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (tx_end, rx_end) = mpsc::unbounded_channel::<()>();
    let pb_style = ProgressStyle::with_template("{prefix:.blue.bold} {wide_msg}").unwrap();
    let pb_style_spinner = ProgressStyle::with_template("{spinner} {prefix:.blue.bold} {wide_msg}")
        .unwrap()
        .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ");
    tokio::spawn(async move {
        let multipb_installing = MultiProgress::new();
        let mut pb_checking_local_plugins = None;
        let mut pb_installskipped = None;
        let mut installskipped_count = 0;
        let mut pb_installyank = None;
        let mut yankfile_count = 0;
        let multipb_caching = MultiProgress::new();
        let mut pb_caching = HashMap::new();
        while let Some(msg) = rx.recv().await {
            match msg {
                Message::DetectConfigFile(path) => {
                    println!("{}", path.to_string_lossy().dimmed());
                }
                Message::CheckingLocalPlugins { install, update } => {
                    let pb = ProgressBar::new_spinner();
                    pb.set_style(pb_style_spinner.clone());
                    pb.set_prefix("Checking");
                    let activity = if install && update {
                        "installed plugins & updates"
                    } else if update {
                        "updates"
                    } else if install {
                        "installed plugins"
                    } else {
                        "local plugins"
                    };
                    pb.set_message(activity);
                    pb.enable_steady_tick(Duration::from_millis(100));
                    pb_checking_local_plugins = Some(pb);
                }
                Message::CheckingLocalPluginsFinished { total, merged } => {
                    let message = format!(
                        "plugins {}",
                        format!("(total:{total} merged:{merged})").green().dimmed()
                    );
                    if let Some(pb) = pb_checking_local_plugins.take() {
                        pb.set_style(pb_style.clone());
                        pb.set_prefix("Loaded");
                        pb.finish_with_message(message);
                    } else {
                        println!("{} {message}", "Loaded".blue().bold());
                    }
                }
                Message::Cache(r#type, url) => {
                    if let Some(pb) = pb_checking_local_plugins.take() {
                        pb.finish_and_clear();
                    }

                    let pb = pb_caching.entry(r#type).or_insert_with(|| {
                        multipb_caching.add(
                            ProgressBar::no_length()
                                .with_style(pb_style.clone())
                                .with_prefix(r#type),
                        )
                    });
                    pb.set_message(url.to_string());
                }
                Message::CacheDone => {
                    multipb_caching.clear().unwrap();
                }
                Message::InstallSkipped(id) => {
                    installskipped_count += 1;
                    let pb = pb_installskipped.get_or_insert_with(|| {
                        multipb_installing.add(
                            ProgressBar::no_length()
                                .with_style(pb_style.clone())
                                .with_prefix("Skipped"),
                        )
                    });
                    pb.set_message(format!("{}", id.italic().dimmed()));
                }
                Message::InstallYank { id, which: file } => {
                    yankfile_count += 1;
                    let pb = pb_installyank.get_or_insert_with(|| {
                        multipb_installing.add(
                            ProgressBar::no_length()
                                .with_style(pb_style.clone())
                                .with_prefix("Copying"),
                        )
                    });
                    pb.set_message(format!(
                        "in {}: {}",
                        id.italic().dimmed(),
                        file.to_string_lossy()
                    ));
                }
                Message::InstallDone => {
                    if let Some(pb) = pb_installskipped.take() {
                        pb.set_style(ProgressStyle::with_template("{wide_msg}").unwrap());
                        pb.finish_with_message(format!("skipped {installskipped_count} packages"));
                    }
                    if let Some(pb) = pb_installyank.take() {
                        pb.set_style(ProgressStyle::with_template("{wide_msg}").unwrap());
                        if yankfile_count != 0 {
                            pb.finish_with_message(format!("copied {yankfile_count} files"));
                        } else {
                            pb.finish_and_clear();
                        }
                    }
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

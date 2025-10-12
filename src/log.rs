use console::style;
use hashbrown::{HashMap, hash_map::Entry};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use once_cell::sync::Lazy;
use std::{
    borrow::Cow,
    io::Write,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::{Mutex, mpsc};
use unicode_width::UnicodeWidthStr;

pub enum Message {
    DetectConfigFile(PathBuf),
    Loading {
        install: bool,
        update: bool,
    },
    Cache(&'static str, Arc<str>),
    CacheFetchObjectsProgress {
        id: String,
        total_objs_count: usize,
        received_objs_count: usize,
    },
    CacheBuildProgress {
        id: String,
        stdtype: usize,
        line: String,
    },
    LoadDone,
    MergeFinished {
        total: usize,
        merged: usize,
    },
    InstallSkipped(Arc<str>),
    InstallYank {
        id: Arc<str>,
        which: PathBuf,
    },
    InstallDone,
    Error(Box<dyn std::error::Error + 'static + Send + Sync>),
}

type MessageSender = RwLock<Option<mpsc::UnboundedSender<Message>>>;
type LoggerCloser = Mutex<mpsc::UnboundedReceiver<()>>;
type Logger = (MessageSender, LoggerCloser);

static LOGGER: Lazy<Logger> = Lazy::new(init);

const CACHE_FETCH_PROGRESS_ID: &str = "KksvT9lv";

fn init() -> Logger {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (tx_end, rx_end) = mpsc::unbounded_channel::<()>();
    let pb_style = ProgressStyle::with_template("{prefix:.blue.bold} {wide_msg}").unwrap();
    let pb_style_spinner =
        ProgressStyle::with_template("{spinner} {prefix:.blue.bold} {wide_msg}").unwrap();
    let pb_style_bar = ProgressStyle::with_template(
        "{spinner} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos:>7}/{len:7}",
    )
    .unwrap()
    .progress_chars("#>-");
    tokio::spawn(async move {
        let multipb_installing = MultiProgress::new();
        let mut pb_checking_local_plugins = None;
        let mut pb_installskipped = None;
        let mut pb_installyank = None;
        let mut installskipped_count = 0;
        let mut yankfile_count = 0;
        let multipb_caching = MultiProgress::new();
        let mut cachefetching_oids: HashMap<String, usize> = HashMap::new();
        let mut pb_caching: HashMap<Cow<'static, str>, _> = HashMap::new();
        while let Some(msg) = rx.recv().await {
            match msg {
                Message::DetectConfigFile(path) => {
                    eprintln!("{}", style(path.to_string_lossy()).dim());
                }
                Message::Loading { install, update } => {
                    let pb = ProgressBar::new_spinner();
                    pb.set_style(pb_style_spinner.clone());
                    pb.set_prefix("Loading");
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
                Message::MergeFinished { total, merged } => {
                    let message = format!(
                        "plugins {}",
                        style(format!("(total:{total} merged:{merged})"))
                            .green()
                            .dim()
                    );
                    if let Some(pb) = pb_checking_local_plugins.take() {
                        pb.set_style(pb_style.clone());
                        pb.set_prefix("Loaded");
                        pb.finish_with_message(message);
                    } else {
                        eprintln!("{} {message}", style("Loaded").blue().bold());
                    }
                }
                Message::Cache(r#type, url) => {
                    if let Some(pb) = pb_checking_local_plugins.take() {
                        pb.finish_and_clear();
                    }

                    let pb = pb_caching.entry(r#type.into()).or_insert_with(|| {
                        multipb_caching.add(
                            ProgressBar::no_length()
                                .with_style(pb_style.clone())
                                .with_prefix(r#type),
                        )
                    });
                    pb.set_message(url.to_string());
                }
                Message::CacheFetchObjectsProgress {
                    id,
                    total_objs_count,
                    received_objs_count,
                } => {
                    let pb = pb_caching
                        .entry(CACHE_FETCH_PROGRESS_ID.into())
                        .or_insert_with(|| {
                            multipb_caching
                                .add(ProgressBar::new(0).with_style(pb_style_bar.clone()))
                        });
                    let entry = cachefetching_oids.entry(id);
                    if let Entry::Vacant(_) = entry {
                        pb.inc_length(total_objs_count as u64);
                    }
                    let prev = entry.or_default();
                    let increment = received_objs_count.saturating_sub(*prev);
                    *prev = received_objs_count;
                    pb.inc(increment as u64);
                }
                Message::CacheBuildProgress { id, stdtype, line } => {
                    let pb = ProgressBar::no_length()
                        .with_style(pb_style_spinner.clone())
                        .with_prefix({
                            let mut prefix = format!("Building [{id}]");
                            // Manual width alignment so that the style template does not need to be changed.
                            const MAX_PREFIX_WIDTH: usize = 30;
                            prefix.push_str(
                                &" ".repeat(MAX_PREFIX_WIDTH.saturating_sub(prefix.width())),
                            );
                            prefix
                        });
                    let pb = pb_caching
                        .entry({
                            let mut id = id;
                            id.push_str(" - Build"); // IDは人間が任意に決めるので、PREFIXを含めて衝突しないようにする
                            id.into()
                        })
                        .or_insert_with(|| multipb_caching.add(pb));
                    pb.set_message(format!(
                        "{} {line}",
                        style({
                            let mut prefix = stdtype.to_string();
                            prefix.push('>');
                            prefix
                        })
                        .dim()
                    ));
                }
                Message::LoadDone => {
                    let mut pb_caching = std::mem::take(&mut pb_caching);
                    if let Some(pb) = pb_caching.remove(CACHE_FETCH_PROGRESS_ID) {
                        pb.set_style(pb_style.clone());
                        pb.finish_and_clear();
                    }
                    for pb in pb_caching.into_values() {
                        pb.set_style(pb_style.clone());
                        pb.finish_with_message("done");
                    }
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
                    pb.set_message(format!("{}", style(id).italic().dim()));
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
                        style(id).italic().dim(),
                        file.to_string_lossy()
                    ));
                }
                Message::InstallDone => {
                    if let Some(pb) = pb_installskipped.take() {
                        pb.set_style(pb_style.clone());
                        if installskipped_count != 0 {
                            pb.set_prefix("Skipped");
                            pb.finish_with_message(format!("{installskipped_count} packages"));
                        } else {
                            pb.finish_and_clear();
                        }
                    }
                    if let Some(pb) = pb_installyank.take() {
                        pb.set_style(pb_style.clone());
                        pb.finish_and_clear();
                        if yankfile_count != 0 {
                            pb.set_prefix("Copied");
                            pb.finish_with_message(format!("{yankfile_count} files"));
                        } else {
                            pb.finish_and_clear();
                        }
                    }
                    // multipb_installing.clear().unwrap();
                }
                Message::Error(e) => {
                    eprintln!("{} {e}", style("error:").red().bold());
                }
            }
        }
        let _ = tx_end.send(());
    });
    (Some(tx).into(), rx_end.into())
}

/// Output log messages
pub fn msg(message: Message) {
    let _ = LOGGER
        .0
        .read()
        .unwrap()
        .as_ref() // 下記closeなどで破棄された際はLOGGERが取得できなくなるので、その時はmessageを揉み消す
        .and_then(|lgr| lgr.send(message).ok());
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
        std::io::stdout().flush().unwrap();
        std::process::exit(code);
    }
}

use console::style;
use hashbrown::{HashMap, hash_map::Entry};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use once_cell::sync::Lazy;
use std::{
    fmt::{Display, Formatter, Result as FmtResult},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::{Mutex, mpsc};
use unicode_width::UnicodeWidthStr;

pub enum Message {
    ConfigFound(Arc<Path>),
    ConfigWalkFinish,
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
    CacheBuildFinished {
        id: String,
        success: bool,
    },
    LoadDone,
    MergeFinished {
        total: usize,
        merged: usize,
    },
    DetectLockFile(PathBuf),
    InstallSkipped(Arc<str>),
    InstallYank {
        id: Arc<str>,
        which: PathBuf,
    },
    InstallHelp {
        help_dir: PathBuf,
    },
    InstallDone,
    Error(Box<dyn std::error::Error + 'static + Send + Sync>),
}

type MessageSender = RwLock<Option<mpsc::UnboundedSender<Message>>>;
type LoggerCloser = Mutex<mpsc::UnboundedReceiver<()>>;
type Logger = (MessageSender, LoggerCloser);

static LOGGER: Lazy<Logger> = Lazy::new(init);

const CACHE_FETCH_PROGRESS_ID: &str = "cache_fetch_progress";
const CACHE_FETCH_STAGE_ID: &str = "cache_fetch_stage";

#[derive(Clone)]
struct ConfigGroup {
    location: Arc<Path>,
    files: Vec<Arc<Path>>,
}

impl ConfigGroup {
    fn new(location: Arc<Path>, mut files: Vec<Arc<Path>>) -> Self {
        files.sort();
        Self { location, files }
    }

    fn location_label(&self) -> String {
        let name = self
            .location
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if name.is_empty() {
            self.location.to_string_lossy().to_string()
        } else {
            name.to_string()
        }
    }

    fn join_names(&self) -> String {
        let mut names: Vec<String> = self
            .files
            .iter()
            .map(|path| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        names.sort();
        names.join(" ")
    }
}

fn config_n_files_string(n: usize) -> String {
    format!(
        "{} {} file{}",
        style("Config").blue().bold(),
        n,
        if n == 1 { "" } else { "s" }
    )
}

struct ConfigList {
    files: Vec<Arc<Path>>,
}

impl ConfigList {
    fn from_files(mut files: Vec<Arc<Path>>) -> Self {
        files.sort();
        Self { files }
    }

    fn render_lines(&self) -> Vec<String> {
        let total = self.files.len();
        let groups = self.build_groups();

        if groups.len() == total {
            let mut lines = Vec::with_capacity(total + 2);
            lines.push(format!(
                "{} ({} locations)",
                config_n_files_string(total),
                total
            ));
            for path in self.files.iter() {
                lines.push(format!("  {}", path.to_string_lossy()));
            }
            return lines;
        }

        if let Some((dominant_idx, dominant_count)) = self.dominant_group(&groups) {
            let main_ratio = if total == 0 {
                0.0
            } else {
                dominant_count as f32 / total as f32
            };
            let external_count = total.saturating_sub(dominant_count);
            if main_ratio >= 0.75 && external_count <= 5 {
                let main = &groups[dominant_idx];
                let mut lines = Vec::new();
                lines.push(config_n_files_string(total));
                lines.push(format!(
                    "    {} ({})",
                    main.location.to_string_lossy(),
                    main.files.len()
                ));
                lines.push(format!("        {}", main.join_names()));
                if external_count != 0 {
                    let suffix = if external_count == 1 { "file" } else { "files" };
                    lines.push(format!("    +{} external {}", external_count, suffix));
                }
                return lines;
            }
        }

        if groups.len() == 1 {
            let group = &groups[0];
            return vec![
                config_n_files_string(total),
                format!("    {}", group.location.to_string_lossy()),
                format!("        {}", group.join_names()),
            ];
        }

        let mut lines = Vec::new();
        lines.push(format!(
            "{} in {} locations",
            config_n_files_string(total),
            groups.len()
        ));
        for group in groups {
            lines.push(format!(
                "    {} ({})",
                group.location_label(),
                group.files.len()
            ));
            lines.push(format!("    {}", group.location.to_string_lossy()));
            lines.push(format!("        {}", group.join_names()));
        }
        while lines.last().map(|line| line.is_empty()).unwrap_or(false) {
            lines.pop();
        }
        lines
    }

    fn build_groups(&self) -> Vec<ConfigGroup> {
        let mut groups_map: HashMap<Arc<Path>, Vec<Arc<Path>>> = HashMap::new();
        for path in self.files.iter() {
            let key = path.parent().unwrap_or(Path::new("/")).into();
            groups_map.entry(key).or_default().push(path.clone());
        }
        let mut groups: Vec<ConfigGroup> = groups_map
            .into_iter()
            .map(|(location, files)| ConfigGroup::new(location, files))
            .collect();
        groups.sort_by(|a, b| a.location.cmp(&b.location));
        groups
    }

    fn dominant_group(&self, groups: &[ConfigGroup]) -> Option<(usize, usize)> {
        let mut dominant_idx = None;
        let mut dominant_count = 0;
        for (idx, group) in groups.iter().enumerate() {
            if group.files.len() > dominant_count {
                dominant_count = group.files.len();
                dominant_idx = Some(idx);
            }
        }
        dominant_idx.map(|idx| (idx, dominant_count))
    }
}

impl Display for ConfigList {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        for line in self.render_lines() {
            writeln!(f, "{}", style(line).dim())?;
        }
        Ok(())
    }
}

struct ProgressManager {
    multipb: MultiProgress,
    pb_style: ProgressStyle,
    pb_style_spinner: ProgressStyle,
    pb_style_bar: ProgressStyle,

    // State
    progress_bars: HashMap<String, BarState>,
    installskipped_count: usize,
    yankfile_count: usize,
    cachefetching_oids: HashMap<String, usize>,
    cache_updating_fetching: HashMap<String, ()>,
    cache_updating_current: Option<String>,
    updating_bar: Option<BarState>,
    cache_fetch_stage: Option<&'static str>,
    config_files: Vec<Arc<Path>>,
}

struct BarState {
    bar: ProgressBar,
    last_message: Option<String>,
}

impl BarState {
    fn new(bar: ProgressBar) -> Self {
        Self {
            bar,
            last_message: None,
        }
    }

    fn set_message_if_changed(&mut self, message: impl Into<String>) {
        let message = message.into();
        if self.last_message.as_deref() == Some(message.as_str()) {
            return;
        }
        self.bar.set_message(message.clone());
        self.last_message = Some(message);
    }
}

fn sanitize_build_line(line: &str) -> Option<String> {
    // Some build tools redraw a single line using '\r' without '\n'.
    // Keep only the last non-empty segment to reduce flicker.
    let mut last = "";
    for part in line.split('\r') {
        if !part.trim().is_empty() {
            last = part;
        }
    }
    let trimmed = last.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.replace('\n', " "))
}

impl ProgressManager {
    fn new() -> Self {
        let pb_style = ProgressStyle::with_template("{prefix:.blue.bold} {wide_msg}").unwrap();
        let pb_style_spinner =
            ProgressStyle::with_template("{spinner} {prefix:.blue.bold} {wide_msg}").unwrap();
        let pb_style_bar = ProgressStyle::with_template(
            "{spinner} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos:>7}/{len:7}",
        )
        .unwrap()
        .progress_chars("#>-");

        let multipb = MultiProgress::new();
        multipb.set_draw_target(ProgressDrawTarget::stderr());
        let mut barstate = BarState::new(
            multipb.add(
                ProgressBar::no_length()
                    .with_style(pb_style.clone())
                    .with_prefix("Config"),
            ),
        );
        barstate.set_message_if_changed("0 files");
        Self {
            multipb,
            pb_style,
            pb_style_spinner,
            pb_style_bar,
            progress_bars: HashMap::from([("config_files".to_string(), barstate)]),
            installskipped_count: 0,
            yankfile_count: 0,
            cachefetching_oids: HashMap::new(),
            cache_updating_fetching: HashMap::new(),
            cache_updating_current: None,
            updating_bar: None,
            cache_fetch_stage: None,
            config_files: Vec::new(),
        }
    }

    fn ensure_fetch_stage(&mut self, stage: &'static str, url: &str) {
        let bar_state = self
            .progress_bars
            .entry(CACHE_FETCH_STAGE_ID.to_string())
            .or_insert_with(|| {
                let pb = ProgressBar::new_spinner().with_style(self.pb_style_spinner.clone());
                pb.enable_steady_tick(Duration::from_millis(100));
                BarState::new(self.multipb.add(pb))
            });
        bar_state.bar.set_prefix(stage);
        bar_state.set_message_if_changed(url.to_string());
        self.cache_fetch_stage = Some(stage);
    }

    fn finish_fetch_stage(&mut self) {
        if let Some(pb) = self.progress_bars.remove(CACHE_FETCH_STAGE_ID) {
            self.cache_fetch_stage = None;
            self.multipb
                .println(format!("{} all packages", style("Fetched").blue().bold()))
                .unwrap();
            pb.bar.set_style(self.pb_style.clone());
            pb.bar.finish_and_clear();
        }
    }

    fn process(&mut self, msg: Message) {
        match msg {
            Message::ConfigFound(path) => {
                let pb = self.progress_bars.get_mut("config_files").unwrap();
                if let Some(path) = path.to_str() {
                    pb.set_message_if_changed(format!(
                        "{} files: {}",
                        self.config_files.len() + 1,
                        style(path).dim(),
                    ));
                }
                self.config_files.push(path);
            }
            Message::ConfigWalkFinish => {
                let mut pb = self.progress_bars.remove("config_files").unwrap();
                pb.set_message_if_changed(format!("{} files", self.config_files.len()));
                pb.bar.finish_and_clear();
                let display = ConfigList::from_files(std::mem::take(&mut self.config_files));
                self.multipb.println(display.to_string()).unwrap();
            }
            Message::MergeFinished { total, merged } => {
                let message = format!(
                    "plugins {}",
                    style(format!("(total:{total} merged:{merged})"))
                        .green()
                        .dim()
                );
                if let Some(pb) = self.progress_bars.remove("loading") {
                    pb.bar.set_style(self.pb_style.clone());
                    pb.bar.set_prefix("Loaded");
                    pb.bar.finish_with_message(message);
                } else {
                    self.multipb
                        .println(format!("{} {message}", style("Loaded").blue().bold()))
                        .unwrap();
                }
            }
            Message::Cache(r#type, url) => {
                if r#type == "Fetching" || r#type == "Initializing" {
                    self.ensure_fetch_stage(r#type, url.as_ref());
                    return;
                }
                self.finish_fetch_stage();

                match r#type {
                    "Updating" | "Updating:done" => {
                        let url = url.to_string();
                        let pb = self.updating_bar.get_or_insert_with(|| {
                            let bar = ProgressBar::new_spinner()
                                .with_style(self.pb_style_spinner.clone())
                                .with_prefix("Updating");
                            bar.enable_steady_tick(Duration::from_millis(100));
                            BarState::new(self.multipb.add(bar))
                        });
                        match r#type {
                            "Updating" => {
                                self.cache_updating_fetching.insert(url.clone(), ());
                                if self.cache_updating_current.is_none() {
                                    self.cache_updating_current = Some(url.clone());
                                    pb.set_message_if_changed(url);
                                }
                            }
                            "Updating:done" => {
                                self.cache_updating_fetching.remove(&url);
                                if self.cache_updating_fetching.is_empty() {
                                    pb.set_message_if_changed(url);
                                    self.cache_updating_current = None;
                                } else if self
                                    .cache_updating_current
                                    .as_deref()
                                    .is_none_or(|c| c == url)
                                {
                                    let next = self.cache_updating_fetching.keys().next().cloned();
                                    self.cache_updating_current = next.clone();
                                    pb.set_message_if_changed(next.unwrap_or_default());
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {
                        let pb = {
                            let style = self.pb_style.clone();
                            let prefix = r#type.to_string();
                            self.progress_bars
                                .entry(r#type.to_string())
                                .or_insert_with(|| {
                                    let bar = self.multipb.add(
                                        ProgressBar::no_length()
                                            .with_style(style)
                                            .with_prefix(prefix),
                                    );
                                    BarState::new(bar)
                                })
                        };
                        pb.set_message_if_changed(url.to_string());
                    }
                }
            }
            Message::CacheFetchObjectsProgress {
                id,
                total_objs_count,
                received_objs_count,
            } => {
                let style = self.pb_style_bar.clone();
                let pb = self
                    .progress_bars
                    .entry(CACHE_FETCH_PROGRESS_ID.to_string())
                    .or_insert_with(|| {
                        let pb = ProgressBar::new(0).with_style(style);
                        pb.enable_steady_tick(Duration::from_millis(100));
                        BarState::new(self.multipb.add(pb))
                    });
                let entry = self.cachefetching_oids.entry(id);
                if let Entry::Vacant(_) = entry {
                    pb.bar.inc_length(total_objs_count as u64);
                }
                let prev = entry.or_default();
                let increment = received_objs_count.saturating_sub(*prev);
                *prev = received_objs_count;
                pb.bar.inc(increment as u64);
            }
            Message::CacheBuildProgress { id, stdtype, line } => {
                self.finish_fetch_stage();
                if let Some(sanitized) = sanitize_build_line(&line) {
                    let entry = format!("build-{id}");
                    let prefix = {
                        let mut prefix = format!("Building [{id}]");
                        const MAX_PREFIX_WIDTH: usize = 30;
                        prefix
                            .push_str(&" ".repeat(MAX_PREFIX_WIDTH.saturating_sub(prefix.width())));
                        prefix
                    };
                    let bar = self.progress_bars.entry(entry).or_insert_with(|| {
                        let style = self.pb_style_spinner.clone();
                        let pb = ProgressBar::new_spinner().with_style(style);
                        pb.enable_steady_tick(Duration::from_millis(100));
                        BarState::new(self.multipb.add(pb.with_prefix(prefix.clone())))
                    });
                    let message = format!(
                        "{} {}",
                        style({
                            let mut prefix = stdtype.to_string();
                            prefix.push('>');
                            prefix
                        })
                        .dim(),
                        sanitized
                    );
                    bar.set_message_if_changed(message);
                    bar.bar.set_prefix(prefix);
                }
            }
            Message::CacheBuildFinished { id, success } => {
                let entry = format!("build-{id}");
                if let Some(pb_state) = self.progress_bars.remove(&entry) {
                    pb_state.bar.set_style(self.pb_style.clone());
                    pb_state.bar.set_prefix("Build");
                    if success {
                        pb_state.bar.finish_with_message(format!("success [{id}]"));
                    } else {
                        pb_state.bar.finish_with_message(format!("failed [{id}]"));
                    }
                }
            }
            Message::LoadDone => {
                self.finish_fetch_stage();
                let mut pbs = std::mem::take(&mut self.progress_bars);
                if let Some(pb) = pbs.remove(CACHE_FETCH_PROGRESS_ID) {
                    pb.bar.set_style(self.pb_style.clone());
                    pb.bar.finish_and_clear();
                }
                if let Some(pb) = self.updating_bar.take() {
                    pb.bar.finish_and_clear();
                }
                for (_, pb) in pbs {
                    pb.bar.set_style(self.pb_style.clone());
                    pb.bar.finish_with_message("done");
                }
            }
            Message::DetectLockFile(path) => {
                self.multipb
                    .println(format!(
                        "{} {}",
                        style("LockFile:").blue().dim(),
                        style(path.to_string_lossy()).dim()
                    ))
                    .unwrap();
            }
            Message::InstallSkipped(id) => {
                self.installskipped_count += 1;
                let pb = self
                    .progress_bars
                    .entry("install_skipped".to_string())
                    .or_insert_with(|| {
                        let bar = self.multipb.add(
                            ProgressBar::no_length()
                                .with_style(self.pb_style.clone())
                                .with_prefix("Skipped"),
                        );
                        BarState::new(bar)
                    });
                pb.set_message_if_changed(format!("{}", style(id).italic().dim()));
            }
            Message::InstallYank { id, which: file } => {
                self.yankfile_count += 1;
                let pb = self
                    .progress_bars
                    .entry("install_yank".to_string())
                    .or_insert_with(|| {
                        let bar = self.multipb.add(
                            ProgressBar::no_length()
                                .with_style(self.pb_style.clone())
                                .with_prefix("Copying"),
                        );
                        BarState::new(bar)
                    });
                pb.set_message_if_changed(format!(
                    "in {}: {}",
                    style(id).italic().dim(),
                    file.to_string_lossy()
                ));
            }
            Message::InstallHelp { help_dir } => {
                let pb = self
                    .progress_bars
                    .entry("install_yank".to_string())
                    .or_insert_with(|| {
                        let bar = self.multipb.add(
                            ProgressBar::no_length()
                                .with_style(self.pb_style.clone())
                                .with_prefix(":helptags"),
                        );
                        BarState::new(bar)
                    });
                pb.set_message_if_changed(help_dir.to_string_lossy().into_owned());
            }
            Message::InstallDone => {
                if let Some(pb) = self.progress_bars.remove("install_skipped") {
                    pb.bar.set_style(self.pb_style.clone());
                    if self.installskipped_count != 0 {
                        pb.bar.set_prefix("Skipped");
                        pb.bar
                            .finish_with_message(format!("{} packages", self.installskipped_count));
                    } else {
                        pb.bar.finish_and_clear();
                    }
                }
                if let Some(pb) = self.progress_bars.remove("install_yank") {
                    pb.bar.set_style(self.pb_style.clone());
                    if self.yankfile_count != 0 {
                        pb.bar.set_prefix("Copied");
                        pb.bar
                            .finish_with_message(format!("{} files", self.yankfile_count));
                    } else {
                        pb.bar.finish_and_clear();
                    }
                }
            }
            Message::Error(e) => {
                // To prevent flicker with other progress bars, suspend drawing.
                self.multipb.suspend(|| {
                    eprintln!("{} {e}", style("error:").red().bold());
                });
            }
        }
    }
}

fn init() -> Logger {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (tx_end, rx_end) = mpsc::unbounded_channel::<()>();
    tokio::spawn(async move {
        let mut manager = ProgressManager::new();
        while let Some(msg) = rx.recv().await {
            manager.process(msg);
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

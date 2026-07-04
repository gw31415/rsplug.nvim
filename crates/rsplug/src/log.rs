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

use crate::osc94::OSC94;

pub enum Message {
    ConfigFound(PathBuf),
    ConfigWalkFinish,
    Cache(&'static str, Arc<str>),
    CacheFetchObjectsProgress {
        id: String,
        total_objs_count: usize,
        received_objs_count: usize,
    },
    CacheBuildProgress {
        id: Arc<String>,
        stdtype: usize,
        line: String,
    },
    CacheBuildFinished {
        id: Arc<String>,
        success: bool,
    },
    LoadBegin {
        total: usize,
    },
    LoadPluginDone,
    LoadDone,
    /// フラグなし実行でキャッシュが無くロードできなかった（未インストール）。
    PluginNotInstalled(Arc<str>),
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
        let names: Vec<String> = self
            .files
            .iter()
            .map(|path| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        // ConfigGroup::new でパス順に整列済み
        // names.sort();
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

/// 持続サマリー行の prefix（`✓ Loaded` / `✗ Build` など）。
/// ネスト ANSI を避けるため `pb_style_summary`（prefix 無修飾）と組み合わせる。
fn summary_prefix(label: &str, ok: bool) -> String {
    let mark = if ok {
        style("✓").green().bold().to_string()
    } else {
        style("✗").red().bold().to_string()
    };
    format!("{} {}", mark, style(label).blue().bold())
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
    pb_style_loading: ProgressStyle,
    pb_style_summary: ProgressStyle,

    // State
    progress_bars: HashMap<String, BarState>,
    installskipped_count: usize,
    yankfile_count: usize,
    not_installed: Vec<Arc<str>>,
    cachefetching_oids: HashMap<String, usize>,
    cache_updating_fetching: HashMap<String, ()>,
    cache_updating_current: Option<String>,
    updating_bar: Option<BarState>,
    config_files: Vec<Arc<Path>>,
    osc94: Option<OSC94>,
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
        Self::build(ProgressDrawTarget::stderr())
    }

    /// 描画先を注入可能にしたコンストラクタ（検証用）。本番は new() → stderr。
    #[cfg(test)]
    fn with_draw_target(draw_target: ProgressDrawTarget) -> Self {
        Self::build(draw_target)
    }

    fn build(draw_target: ProgressDrawTarget) -> Self {
        let pb_style = ProgressStyle::with_template("{prefix:.blue.bold} {wide_msg}").unwrap();
        let pb_style_spinner =
            ProgressStyle::with_template("{spinner} {prefix:.blue.bold} {wide_msg}")
                .unwrap()
                .tick_strings(&["◒", "◐", "◓", "◑", " "]);
        let pb_style_bar = ProgressStyle::with_template(
            "{spinner} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos:>7}/{len:7}",
        )
        .unwrap()
        .progress_chars("■□ ");
        let pb_style_loading = ProgressStyle::with_template(
            "{spinner} {prefix:.blue.bold} {wide_bar:.cyan/blue} {pos}/{len}",
        )
        .unwrap()
        .progress_chars("■□ ")
        .tick_strings(&["◒", "◐", "◓", "◑", " "]);
        // prefix を自前で色付けする（✓/✗ 付きの持続サマリー行用）。テキストはそのまま通る。
        let pb_style_summary = ProgressStyle::with_template("{prefix} {wide_msg}").unwrap();

        let multipb = MultiProgress::new();
        multipb.set_draw_target(draw_target);
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
            pb_style_loading,
            pb_style_summary,
            progress_bars: HashMap::from([("config_files".to_string(), barstate)]),
            installskipped_count: 0,
            yankfile_count: 0,
            not_installed: Vec::new(),
            cachefetching_oids: HashMap::new(),
            cache_updating_fetching: HashMap::new(),
            cache_updating_current: None,
            updating_bar: None,
            config_files: Vec::new(),
            osc94: None,
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
    }

    fn clear_fetch_stage(&mut self) {
        if let Some(pb) = self.progress_bars.remove(CACHE_FETCH_STAGE_ID) {
            pb.bar.finish_and_clear();
        }
    }

    /// フラグなし実行でキャッシュが無くロードできなかったプラグインの警告を印字。
    fn warn_not_installed(&self) {
        let n = self.not_installed.len();
        let header = format!(
            "{} {} plugins not installed (run with -i to install)",
            style("⚠").yellow().bold(),
            n
        );
        let shown: Vec<&Arc<str>> = self.not_installed.iter().take(3).collect();
        let mut body = shown
            .iter()
            .map(|s| s.as_ref().to_string())
            .collect::<Vec<_>>()
            .join(" · ");
        if n > shown.len() {
            body.push_str(" …");
        }
        let block = if body.is_empty() {
            header
        } else {
            format!("{header}\n    {}", style(body).dim())
        };
        self.multipb.println(block).unwrap();
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
                self.config_files.push(path.into());
            }
            Message::ConfigWalkFinish => {
                let mut pb = self.progress_bars.remove("config_files").unwrap();
                pb.set_message_if_changed(format!("{} files", self.config_files.len()));
                pb.bar.finish_and_clear();
                let display = ConfigList::from_files(std::mem::take(&mut self.config_files));
                self.multipb.println(display.to_string()).unwrap();
            }
            Message::MergeFinished { total, merged } => {
                // total = ロード成功プラグイン数、merged = マージ後のパッケージ数。
                // 統合（merged < total）のときだけ 📦 で結果パッケージ数を併記。
                let message = if total == merged {
                    format!("{} plugins", style(total).green().bold())
                } else {
                    format!(
                        "{} plugins {} {}",
                        style(total).green().bold(),
                        style("·").dim(),
                        style(format!("📦 {}", merged)).green().bold(),
                    )
                };
                if let Some(pb) = self.progress_bars.remove("loading") {
                    // steady_tick は LoadDone（描画責務）で停止済み。ここはサマリー確定の
                    // 責務としてバーを消去し、サマリー行を println で1行出す。
                    pb.bar.finish_and_clear();
                }
                self.multipb
                    .println(format!("{} {message}", summary_prefix("Loaded", true)))
                    .unwrap();
            }
            Message::Cache(r#type, url) => {
                if r#type == "Fetching" || r#type == "Initializing" {
                    self.ensure_fetch_stage(r#type, url.as_ref());
                    return;
                }

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
                // OSC 9;4: フェッチ中は全体オブジェクト比(受信/総数)、完了時は Loading(不定)。
                let pos = pb.bar.position();
                let len = pb.bar.length().unwrap_or(pos).max(1);
                if pos < len {
                    self.osc94
                        .get_or_insert_with(OSC94::new)
                        .progress(Some((pos * 100 / len).min(100) as u8));
                } else {
                    self.osc94
                        .get_or_insert_with(OSC94::new)
                        .progress::<u8>(None);
                }
            }
            Message::CacheBuildProgress { id, stdtype, line } => {
                if let Some(sanitized) = sanitize_build_line(&line) {
                    let prefix = {
                        let mut prefix = format!("Building [{id}]");
                        const MAX_PREFIX_WIDTH: usize = 30;
                        prefix
                            .push_str(&" ".repeat(MAX_PREFIX_WIDTH.saturating_sub(prefix.width())));
                        prefix
                    };
                    let bar = self.progress_bars.entry(id.to_string()).or_insert_with(|| {
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
                if let Some(pb_state) = self.progress_bars.remove(id.as_str()) {
                    pb_state.bar.set_style(self.pb_style_summary.clone());
                    pb_state.bar.set_prefix(summary_prefix("Build", success));
                    if success {
                        pb_state.bar.finish_with_message(format!("[{id}]"));
                    } else {
                        pb_state.bar.finish_with_message(format!("[{id}] failed"));
                    }
                }
            }
            Message::LoadBegin { total } => {
                let pb = ProgressBar::new(total as u64)
                    .with_style(self.pb_style_loading.clone())
                    .with_prefix("Loading");
                pb.enable_steady_tick(Duration::from_millis(120));
                let bar = BarState::new(self.multipb.add(pb));
                self.progress_bars.insert("loading".to_string(), bar);
                // Loading フェーズは OSC 9;4 を不定(割合なし)にする。
                self.osc94
                    .get_or_insert_with(OSC94::new)
                    .progress::<u8>(None);
            }
            Message::LoadPluginDone => {
                if let Some(state) = self.progress_bars.get_mut("loading") {
                    state.bar.inc(1);
                }
            }
            Message::LoadDone => {
                self.clear_fetch_stage();
                drop(self.osc94.take());
                let mut pbs = std::mem::take(&mut self.progress_bars);
                if let Some(pb) = pbs.remove(CACHE_FETCH_PROGRESS_ID) {
                    pb.bar.set_style(self.pb_style.clone());
                    pb.bar.finish_and_clear();
                }
                if let Some(pb) = self.updating_bar.take() {
                    pb.bar.finish_and_clear();
                }
                for (key, pb) in pbs {
                    // "loading" は MergeFinished でサマリーに差し替えるまで残す。
                    if key == "loading" {
                        // 描画(アニメーション)の責務は loading フェーズの完了(LoadDone)で終わり。
                        // ここで steady_tick を止め、バーは最終進捗を表示したまま残す。
                        // MergeFinished との間に lockfile 書き出し + merge が挟まるため、
                        // この時点で ticker スレッドは確実に停止し、MergeFinished での
                        // finish_and_clear が競合しない（"Loading 0/len" の初期フレーム残存を防ぐ）。
                        pb.bar.disable_steady_tick();
                        self.progress_bars.insert(key, pb);
                        continue;
                    }
                    pb.bar.finish_and_clear();
                }
                if !self.not_installed.is_empty() {
                    self.warn_not_installed();
                }
            }
            Message::PluginNotInstalled(id) => {
                self.not_installed.push(id);
            }
            Message::DetectLockFile(path) => {
                self.multipb
                    .println(format!(
                        "{} {}",
                        summary_prefix("Lockfile", true),
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
                                .with_prefix("Up to date"),
                        );
                        BarState::new(bar)
                    });
                pb.set_message_if_changed(format!("{}", style(id).italic().dim()));
            }
            Message::InstallYank { id, which: file } => {
                self.yankfile_count += 1;
                self.osc94
                    .get_or_insert_with(OSC94::new)
                    .progress::<u8>(None);
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
                drop(self.osc94.take());
                if let Some(pb) = self.progress_bars.remove("install_skipped") {
                    pb.bar.set_style(self.pb_style_summary.clone());
                    if self.installskipped_count != 0 {
                        pb.bar.set_prefix(summary_prefix("Up to date", true));
                        pb.bar
                            .finish_with_message(format!("{} packages", self.installskipped_count));
                    } else {
                        pb.bar.finish_and_clear();
                    }
                }
                if let Some(pb) = self.progress_bars.remove("install_yank") {
                    pb.bar.set_style(self.pb_style_summary.clone());
                    if self.yankfile_count != 0 {
                        pb.bar.set_prefix(summary_prefix("Copied", true));
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

#[cfg(test)]
mod tests {
    use super::*;
    use indicatif::TermLike;
    use std::io;
    use std::sync::{Arc, Mutex};

    /// 画面モデルを模倣し、indicatif のカーソル操作を再現して最終画面状態を得る TermLike。
    /// 行(row)追跡は正確に行い、`finish_and_clear`+`println` で "Loading" 行が残留しないか検証する。
    #[derive(Debug)]
    struct Screen {
        rows: Vec<String>,
        row: usize,
        col: usize,
    }

    #[derive(Debug, Clone)]
    struct ScreenTermLike {
        screen: Arc<Mutex<Screen>>,
        width: u16,
    }

    impl ScreenTermLike {
        fn new(width: u16) -> (Self, Arc<Mutex<Screen>>) {
            let screen = Arc::new(Mutex::new(Screen {
                rows: Vec::new(),
                row: 0,
                col: 0,
            }));
            (
                Self {
                    screen: screen.clone(),
                    width,
                },
                screen,
            )
        }
    }

    impl TermLike for ScreenTermLike {
        fn width(&self) -> u16 {
            self.width
        }
        fn height(&self) -> u16 {
            200
        }
        fn move_cursor_up(&self, n: usize) -> io::Result<()> {
            let mut s = self.screen.lock().unwrap();
            s.row = s.row.saturating_sub(n);
            Ok(())
        }
        fn move_cursor_down(&self, n: usize) -> io::Result<()> {
            let mut s = self.screen.lock().unwrap();
            s.row += n;
            while s.rows.len() <= s.row {
                s.rows.push(String::new());
            }
            Ok(())
        }
        fn move_cursor_right(&self, n: usize) -> io::Result<()> {
            self.screen.lock().unwrap().col += n;
            Ok(())
        }
        fn move_cursor_left(&self, n: usize) -> io::Result<()> {
            let mut s = self.screen.lock().unwrap();
            s.col = s.col.saturating_sub(n);
            Ok(())
        }
        fn write_str(&self, text: &str) -> io::Result<()> {
            let mut s = self.screen.lock().unwrap();
            for c in text.chars() {
                match c {
                    '\r' => s.col = 0,
                    '\n' => {
                        s.row += 1;
                        s.col = 0;
                        while s.rows.len() <= s.row {
                            s.rows.push(String::new());
                        }
                    }
                    _ => {
                        let (row, col) = (s.row, s.col);
                        while s.rows.len() <= row {
                            s.rows.push(String::new());
                        }
                        let r = &mut s.rows[row];
                        while r.chars().count() < col {
                            r.push(' ');
                        }
                        // col を文字数で管理する（ANSI 幅は無視: 残留検出には行位置で十分）
                        let mut chars: Vec<char> = r.chars().collect();
                        while chars.len() <= col {
                            chars.push(' ');
                        }
                        chars[col] = c;
                        *r = chars.into_iter().collect();
                        s.col += 1;
                    }
                }
            }
            Ok(())
        }
        fn write_line(&self, text: &str) -> io::Result<()> {
            self.write_str(text)?;
            let mut s = self.screen.lock().unwrap();
            s.row += 1;
            s.col = 0;
            while s.rows.len() <= s.row {
                s.rows.push(String::new());
            }
            Ok(())
        }
        fn clear_line(&self) -> io::Result<()> {
            let mut s = self.screen.lock().unwrap();
            let row = s.row;
            while s.rows.len() <= row {
                s.rows.push(String::new());
            }
            s.rows[row].clear();
            s.col = 0;
            Ok(())
        }
        fn flush(&self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Loading bar のライフサイクル（責務分離）: LoadBegin で生成→LoadDone で frozen
    /// （steady_tick 停止・未 finish）→MergeFinished で消去。
    #[test]
    fn loading_bar_freezes_at_load_done_and_clears_at_merge_finished() {
        let mut m = ProgressManager::new();

        m.process(Message::LoadBegin { total: 2 });
        let loading = m
            .progress_bars
            .get("loading")
            .expect("LoadBegin creates the loading bar");
        assert!(!loading.bar.is_finished());

        m.process(Message::LoadPluginDone);
        m.process(Message::LoadPluginDone);

        m.process(Message::LoadDone);
        let loading = m
            .progress_bars
            .get("loading")
            .expect("LoadDone must keep the loading bar for MergeFinished");
        // frozen だが未 finish: MergeFinished で消去されるまでは表示し続ける。
        assert!(!loading.bar.is_finished());

        m.process(Message::MergeFinished {
            total: 2,
            merged: 1,
        });
        assert!(
            !m.progress_bars.contains_key("loading"),
            "MergeFinished must remove the loading bar"
        );
    }

    /// `-u` 相当のメッセージ列を流した最終画面に "Loading" が残留しないか検証する。
    /// LoadDone→MergeFinished の間（lockfile 書き出し+merge に相当）に steady_tick が
    /// 回り続けると finish_and_clear と競合して初期フレームが残るため、責務分離で
    /// LoadDone 時点で steady_tick を止める。このテストはその最終画面がクリーンであることを保証する。
    #[test]
    fn loading_bar_does_not_remain_on_screen_after_update_run() {
        let (term, screen) = ScreenTermLike::new(80);
        let mut m =
            ProgressManager::with_draw_target(ProgressDrawTarget::term_like(Box::new(term)));

        m.process(Message::ConfigWalkFinish);

        m.process(Message::LoadBegin { total: 2 });

        let mk = |s: &str| Arc::<str>::from(s);
        // plugin 1: update + fetch + done
        let u1 = mk("file:///x/a");
        m.process(Message::Cache("Updating", u1.clone()));
        m.process(Message::Cache("Updating:done", u1.clone()));
        m.process(Message::Cache("Fetching", u1.clone()));
        m.process(Message::CacheFetchObjectsProgress {
            id: "a".into(),
            total_objs_count: 1,
            received_objs_count: 1,
        });
        m.process(Message::LoadPluginDone);
        // plugin 2
        let u2 = mk("file:///x/b");
        m.process(Message::Cache("Updating", u2.clone()));
        m.process(Message::Cache("Updating:done", u2.clone()));
        m.process(Message::LoadPluginDone);

        // lockfile 書き出し + merge に相当する時間（steady_tick が回る窓を再現）
        std::thread::sleep(Duration::from_millis(160));

        m.process(Message::LoadDone);
        // MergeFinished 直前も steady_tick 窓がありうる
        std::thread::sleep(Duration::from_millis(40));
        m.process(Message::MergeFinished {
            total: 2,
            merged: 1,
        });

        // 残存 ticker の非同期 tick が落ち着くまで待つ
        std::thread::sleep(Duration::from_millis(160));

        let rendered = {
            let s = screen.lock().unwrap();
            s.rows
                .iter()
                .map(|r| console::strip_ansi_codes(r))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(
            !rendered.contains("Loading"),
            "loading bar must be cleared after the run; got:\n{rendered}"
        );
    }
}

use console::style;
use hashbrown::{HashMap, hash_map::Entry};
use indicatif::{
    MultiProgress, ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle,
    style::ProgressTracker,
};
use once_cell::sync::Lazy;
use std::{
    fmt::{Display, Formatter, Result as FmtResult},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, mpsc};
use unicode_width::UnicodeWidthStr;

use crate::osc94::OSC94;
use crate::rsplug::util::truncate;

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
    /// プラグインが fetch 許可を取得し、ネットワーク実作業に入った（=稼働中 +1）。
    /// `LoadPluginRunningDone` と対で用い、load() の戻りまでを「稼働中」とする。
    LoadPluginRunning,
    /// 稼働中プラグインの完了（=稼働中 -1）。`RunningGuard` の Drop で送出される。
    LoadPluginRunningDone,
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

/// レベル1子バー（Fetching / Updating / Building）の追加順を追跡するキー。
/// 最後の子に `└─`、それ以外に `├─` を割り当てるために使う。
/// MultiProgress は挿入順に描画するため、追加順＝表示順に依存できる。
#[derive(Clone, Debug, PartialEq, Eq)]
enum ChildKey {
    /// Fetching / Initializing ステージ行（`progress_bars[CACHE_FETCH_STAGE_ID]`）。
    FetchStage,
    /// Updating 行（`self.updating_bar`）。
    Updating,
    /// ビルド行（`progress_bars[id]`）。
    Building(Arc<String>),
}

struct ProgressManager {
    multipb: MultiProgress,
    pb_style: ProgressStyle,
    pb_style_spinner_mid: ProgressStyle,
    pb_style_spinner_last: ProgressStyle,
    pb_style_bar_last: ProgressStyle,
    pb_style_bar_last_nolead: ProgressStyle,
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
    /// レベル1子バーの追加順。最後の子の罫線を `└─` に切り替えるために使う。
    child_order: Vec<ChildKey>,
    config_files: Vec<Arc<Path>>,
    osc94: Option<OSC94>,
    /// Loading バーの「稼働中」計数。LoadBegin でバー生成時に作り、
    /// `dualbar`/`active` トラッカーと共有する。LoadDone で破棄。
    loading_running: Option<Arc<AtomicUsize>>,
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

/// Loading 全体進捗バーの固定セル幅。
/// `wide_bar` と違いカスタムトラッカーで自前描画するため、`ProgressState` からは
/// 端末幅が取れない（描画幅はトラッカーに渡されない）。よって固定幅とし、
/// `dual_bar_cells` で確定的に計算してテスト可能にする。
const LOADING_BAR_CELLS: usize = 40;

/// `width` セルのバーを (Done, 稼働中, 未実行) に配分する。
/// 端数は切り捨てて Done→稼働中の順に確保し、合計が常に `width` になるよう
/// 未実行で調整する（稼働中が Done の右隣に連続して描かれる）。
fn dual_bar_cells(done: u64, running: u64, len: u64, width: usize) -> (usize, usize, usize) {
    let width = width as u64;
    let len = len.max(1);
    let done_cells = (done * width / len).min(width) as usize;
    let running_cells = ((running * width / len).min(width - done_cells as u64)) as usize;
    let idle_cells = width as usize - done_cells - running_cells;
    (done_cells, running_cells, idle_cells)
}

/// Loading バー本体: `■`(Done=cyan) + `□`(稼働中=yellow) + (空白=未実行)。
#[derive(Clone)]
struct DualBarTracker {
    running: Arc<AtomicUsize>,
}

impl ProgressTracker for DualBarTracker {
    fn clone_box(&self) -> Box<dyn ProgressTracker> {
        Box::new(self.clone())
    }
    fn tick(&mut self, _state: &ProgressState, _now: Instant) {}
    fn reset(&mut self, _state: &ProgressState, _now: Instant) {}
    fn write(&self, state: &ProgressState, w: &mut dyn std::fmt::Write) {
        let done = state.pos();
        let len = state.len().unwrap_or(done).max(1);
        let running = self.running.load(Ordering::Relaxed) as u64;
        let (done_cells, running_cells, idle_cells) =
            dual_bar_cells(done, running, len, LOADING_BAR_CELLS);
        let _ = write!(
            w,
            "{}{}{}",
            style("■".repeat(done_cells)).cyan(),
            style("□".repeat(running_cells)).yellow(),
            " ".repeat(idle_cells),
        );
    }
}

/// Loading バーの注釈: 稼働中 repo 数（`pos/len` に続けて `N active`）。
#[derive(Clone)]
struct ActiveTracker {
    running: Arc<AtomicUsize>,
}

impl ProgressTracker for ActiveTracker {
    fn clone_box(&self) -> Box<dyn ProgressTracker> {
        Box::new(self.clone())
    }
    fn tick(&mut self, _state: &ProgressState, _now: Instant) {}
    fn reset(&mut self, _state: &ProgressState, _now: Instant) {}
    fn write(&self, _state: &ProgressState, w: &mut dyn std::fmt::Write) {
        let running = self.running.load(Ordering::Relaxed);
        // 先頭の区切り空白はテンプレート側 ({pos}/{len} {active}) が持つ。
        let _ = write!(
            w,
            "{} {}",
            style(running).yellow().bold(),
            style("active").dim(),
        );
    }
}

/// Loading バーの ProgressStyle。`dualbar`(本体) と `active`(稼働中数) を
/// 共有の稼働中計数 `running` に束ねる。spinner / prefix は既存スタイルを維持。
fn loading_style(running: Arc<AtomicUsize>) -> ProgressStyle {
    ProgressStyle::with_template("{spinner} {prefix:.blue.bold} {dualbar} {pos}/{len} {active}")
        .unwrap()
        .with_key(
            "dualbar",
            DualBarTracker {
                running: running.clone(),
            },
        )
        .with_key("active", ActiveTracker { running })
        .tick_strings(&["◒", "◐", "◓", "◑", " "])
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
        // ツリー罫線で階層を表現する:
        //   Loading は親（レベル0）、Fetching/Building/Updating は子（レベル1）、
        //   フェッチオブジェクト進捗は孫（レベル2）。
        let pb_style = ProgressStyle::with_template("{prefix:.blue.bold} {wide_msg}").unwrap();
        // レベル1 子バー。中間は ├─、最後の子は └─（refresh_connectors で切替）。
        let pb_style_spinner_mid =
            ProgressStyle::with_template("{spinner}  ├─ {prefix:.blue.bold} {wide_msg}")
                .unwrap()
                .tick_strings(&["◒", "◐", "◓", "◑", " "]);
        let pb_style_spinner_last =
            ProgressStyle::with_template("{spinner}  └─ {prefix:.blue.bold} {wide_msg}")
                .unwrap()
                .tick_strings(&["◒", "◐", "◓", "◑", " "]);
        // レベル2 孫バー（フェッチオブジェクト進捗、単一）。常に └─。
        // 先頭の │ は親(FetchStage)が最後の子でないときだけ出す。
        let pb_style_bar_last = ProgressStyle::with_template(
            "{spinner}  │  └─ [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos:>7}/{len:7}",
        )
        .unwrap()
        .progress_chars("■□ ")
        .tick_strings(&["◒", "◐", "◓", "◑", " "]);
        // 親(FetchStage)が最後の子のときは │ を空白で段揃え（`  │  `=5セル → 空白5つ）。
        let pb_style_bar_last_nolead = ProgressStyle::with_template(
            "{spinner}     └─ [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos:>7}/{len:7}",
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
            pb_style_spinner_mid,
            pb_style_spinner_last,
            pb_style_bar_last,
            pb_style_bar_last_nolead,
            pb_style_summary,
            progress_bars: HashMap::from([("config_files".to_string(), barstate)]),
            installskipped_count: 0,
            yankfile_count: 0,
            not_installed: Vec::new(),
            cachefetching_oids: HashMap::new(),
            cache_updating_fetching: HashMap::new(),
            cache_updating_current: None,
            updating_bar: None,
            child_order: Vec::new(),
            config_files: Vec::new(),
            osc94: None,
            loading_running: None,
        }
    }

    /// 子キーから対応するアクティブな BarState を引く。
    /// バーが既に除去されている（Map/Option に無い）場合は None。
    fn child_bar(&self, key: &ChildKey) -> Option<&BarState> {
        match key {
            ChildKey::FetchStage => self.progress_bars.get(CACHE_FETCH_STAGE_ID),
            ChildKey::Updating => self.updating_bar.as_ref(),
            ChildKey::Building(id) => self.progress_bars.get(id.as_str()),
        }
    }

    /// 子バーの追加/除去後に呼び、追加順に従い最後の子に └─・それ以外に ├─ を当て直す。
    /// 併せて孫バー(cache_fetch_progress)の罫線も、親(FetchStage)が最後か否かで切り替える。
    /// set_style 自体は即時描画しないが各バーは steady_tick(100ms) で再描画され、
    /// prefix/message は保持される（indicatif 0.18 で確認）。
    fn refresh_connectors(&self) {
        let last = self.child_order.last();
        for key in &self.child_order {
            let Some(bs) = self.child_bar(key) else {
                continue;
            };
            let style = if Some(key) == last {
                self.pb_style_spinner_last.clone()
            } else {
                self.pb_style_spinner_mid.clone()
            };
            bs.bar.set_style(style);
        }
        // 孫バー。親(FetchStage)が最後の子のときだけ先頭の │ を消して段揃えする。
        if let Some(gc) = self.progress_bars.get(CACHE_FETCH_PROGRESS_ID) {
            let fetch_is_last = last == Some(&ChildKey::FetchStage);
            let style = if fetch_is_last {
                self.pb_style_bar_last_nolead.clone()
            } else {
                self.pb_style_bar_last.clone()
            };
            gc.bar.set_style(style);
        }
    }

    /// 新しいレベル1子バーを追加順の末尾に登録し、罫線を再計算する。
    /// 既に同じキーが登録済みなら何もしない（二重 add 防止）。
    fn register_child(&mut self, key: ChildKey) {
        if !self.child_order.contains(&key) {
            self.child_order.push(key);
        }
        self.refresh_connectors();
    }

    /// レベル1子バーを登録から外し、罫線を再計算する。
    fn unregister_child(&mut self, key: &ChildKey) {
        self.child_order.retain(|k| k != key);
        self.refresh_connectors();
    }

    fn ensure_fetch_stage(&mut self, stage: &'static str, url: &str) {
        let existed = self.progress_bars.contains_key(CACHE_FETCH_STAGE_ID);
        let bar_state = self
            .progress_bars
            .entry(CACHE_FETCH_STAGE_ID.to_string())
            .or_insert_with(|| {
                let pb = ProgressBar::new_spinner().with_style(self.pb_style_spinner_mid.clone());
                pb.enable_steady_tick(Duration::from_millis(100));
                BarState::new(self.multipb.add(pb))
            });
        bar_state.bar.set_prefix(stage);
        bar_state.set_message_if_changed(url.to_string());
        if !existed {
            self.register_child(ChildKey::FetchStage);
        }
    }

    fn clear_fetch_stage(&mut self) {
        if let Some(pb) = self.progress_bars.remove(CACHE_FETCH_STAGE_ID) {
            pb.bar.finish_and_clear();
            self.unregister_child(&ChildKey::FetchStage);
        }
    }

    /// フラグなし実行でキャッシュが無くロードできなかったプラグインの警告を印字。
    /// 各 name は logid のような共通 truncate ではなく、**個別に** 20字超のときだけ
    /// truncate する。これにより複数プラグインを並べても何が未インストールか判読できる。
    fn warn_not_installed(&self) {
        let n = self.not_installed.len();
        let header = format!(
            "{} {} plugins not installed (run with -i to install)",
            style("⚠").yellow().bold(),
            n
        );
        const NAME_DISPLAY_LIMIT: usize = 20;
        let shown: Vec<String> = self
            .not_installed
            .iter()
            .take(3)
            .map(|s| truncate(s, NAME_DISPLAY_LIMIT))
            .collect();
        let mut body = shown.join(" · ");
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
                        let existed = self.updating_bar.is_some();
                        let pb = self.updating_bar.get_or_insert_with(|| {
                            let bar = ProgressBar::new_spinner()
                                .with_style(self.pb_style_spinner_mid.clone())
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
                        if !existed {
                            self.register_child(ChildKey::Updating);
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
                // 孫バー（単一）は常に └─。先頭の │ は親(FetchStage)が最後の子でない
                // ときだけ出す。親位置がその後変われば register/unregister の
                // refresh_connectors が追従する。
                let style = if self.child_order.last() == Some(&ChildKey::FetchStage) {
                    self.pb_style_bar_last_nolead.clone()
                } else {
                    self.pb_style_bar_last.clone()
                };
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
                    let existed = self.progress_bars.contains_key(id.as_str());
                    let bar = self.progress_bars.entry(id.to_string()).or_insert_with(|| {
                        let style = self.pb_style_spinner_mid.clone();
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
                    if !existed {
                        self.register_child(ChildKey::Building(id));
                    }
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
                // 終了したビルド行をツリーから外し、残りの最後の子を └─ に再計算する。
                self.unregister_child(&ChildKey::Building(id));
            }
            Message::LoadBegin { total } => {
                // 全体進捗バーは「稼働中」を可視化するため二色セルで描画する:
                //   ■ Done(=pos) / □ 稼働中(=running) / (空白) 未実行
                // 従来は完了数(pos)のみで進んだため、パイプライン充填中や
                // 並列度立ち上がり時にバーが長く 0% で止まり、最後に一気に進んで見えた。
                // 稼働中セルと注釈の「N active」で、作業中の実体を逐次反映する。
                let running = Arc::new(AtomicUsize::new(0));
                let pb = ProgressBar::new(total as u64)
                    .with_style(loading_style(running.clone()))
                    .with_prefix("Loading");
                pb.enable_steady_tick(Duration::from_millis(120));
                let bar = BarState::new(self.multipb.add(pb));
                self.progress_bars.insert("loading".to_string(), bar);
                self.loading_running = Some(running);
                // Loading フェーズは OSC 9;4 を不定(割合なし)にする。
                self.osc94
                    .get_or_insert_with(OSC94::new)
                    .progress::<u8>(None);
            }
            Message::LoadPluginRunning => {
                if let Some(running) = self.loading_running.as_ref() {
                    running.fetch_add(1, Ordering::Relaxed);
                }
            }
            Message::LoadPluginRunningDone => {
                if let Some(running) = self.loading_running.as_ref() {
                    running.fetch_sub(1, Ordering::Relaxed);
                }
            }
            Message::LoadPluginDone => {
                if let Some(state) = self.progress_bars.get_mut("loading") {
                    state.bar.inc(1);
                }
            }
            Message::LoadDone => {
                self.clear_fetch_stage();
                drop(self.osc94.take());
                // これ以降稼働中計数は更新されない（バーは MergeFinished まで残留表示）。
                self.loading_running = None;
                let mut pbs = std::mem::take(&mut self.progress_bars);
                if let Some(pb) = pbs.remove(CACHE_FETCH_PROGRESS_ID) {
                    pb.bar.set_style(self.pb_style.clone());
                    pb.bar.finish_and_clear();
                }
                if let Some(pb) = self.updating_bar.take() {
                    pb.bar.finish_and_clear();
                    self.unregister_child(&ChildKey::Updating);
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
                // 全レベル1子バー(Fetching/Updating/Building)を破棄したので追加順も空にする。
                self.child_order.clear();
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

    /// 未インストール警告で各 name が**個別に** truncate されることを検証する。
    /// logid のような共通切り詰めだと複数プラグインで判読不能になるため、
    /// 長い name（20字超）だけ `……` 付きで切り詰め、短い name は崩さない。
    #[test]
    fn warn_not_installed_truncates_each_name_individually() {
        let (term, screen) = ScreenTermLike::new(120);
        let mut m =
            ProgressManager::with_draw_target(ProgressDrawTarget::term_like(Box::new(term)));

        // 1 つ目は20字超の name（truncate 対象）、2 つ目は短い name（そのまま表示）
        m.process(Message::PluginNotInstalled(Arc::from(
            "abcdefghijklmnopqrstuvwxyz",
        )));
        m.process(Message::PluginNotInstalled(Arc::from("short")));

        m.process(Message::LoadDone);

        let rendered = {
            let s = screen.lock().unwrap();
            s.rows
                .iter()
                .map(|r| console::strip_ansi_codes(r))
                .collect::<Vec<_>>()
                .join("\n")
        };
        // 長い name は個別に truncate されて `……` を含む
        assert!(
            rendered.contains("……"),
            "long name should be truncated individually; got:\n{rendered}"
        );
        // 短い name は崩れずそのまま表示される
        assert!(
            rendered.contains("short"),
            "short name should be shown as-is; got:\n{rendered}"
        );
        // 2 件は ` · ` で結合され、件数ヘッダが付く
        assert!(
            rendered.contains("2 plugins not installed"),
            "header should report the count; got:\n{rendered}"
        );
    }

    /// 4 件以上の未インストールは先頭 3 件を個別 truncate して表示し、
    /// 残りを ` …` で省略することを検証する。
    #[test]
    fn warn_not_installed_ellipsis_when_more_than_three() {
        let (term, screen) = ScreenTermLike::new(120);
        let mut m =
            ProgressManager::with_draw_target(ProgressDrawTarget::term_like(Box::new(term)));

        for s in ["aaaa", "bbbb", "cccc", "dddd"] {
            m.process(Message::PluginNotInstalled(Arc::from(s)));
        }
        m.process(Message::LoadDone);

        let rendered = {
            let s = screen.lock().unwrap();
            s.rows
                .iter()
                .map(|r| console::strip_ansi_codes(r))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(
            rendered.contains("4 plugins not installed"),
            "header should report 4; got:\n{rendered}"
        );
        assert!(
            rendered.contains(" …"),
            "remaining plugins should be elided; got:\n{rendered}"
        );
        // 先頭3件は表示され、4件目は表示されない
        assert!(rendered.contains("aaaa"), "got:\n{rendered}");
        assert!(
            !rendered.contains("dddd"),
            "4th should be hidden; got:\n{rendered}"
        );
    }

    fn screen_rendered(screen: &Arc<Mutex<Screen>>) -> String {
        let s = screen.lock().unwrap();
        s.rows
            .iter()
            .map(|r| console::strip_ansi_codes(r))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 二色バーのセル配分: ■(Done) → □(稼働中) → (空白=未実行) の順に確保し、
    /// 常に合計 = width になること。稼働中は Done の右隣に連続して描かれる。
    #[test]
    fn dual_bar_cells_allocates_done_then_running_then_idle() {
        // 1 cell per plugin (len=10, width=10).
        let (done, running, idle) = dual_bar_cells(3, 2, 10, 10);
        assert_eq!((done, running, idle), (3, 2, 5));
        assert_eq!(done + running + idle, 10);

        // 稼働中が Done の領域を侵さない（Done 優先、残りを稼働中に clamp）。
        let (done, running, idle) = dual_bar_cells(8, 5, 10, 10);
        assert_eq!((done, running, idle), (8, 2, 0));

        // 端数は切り捨て。130 plugins, width 40: done=65 -> 20, running=8 -> 2.
        let (done, running, idle) = dual_bar_cells(65, 8, 130, LOADING_BAR_CELLS);
        assert_eq!((done, running, idle), (20, 2, 18));

        // len=0 でも安全（分母を max(1) で護る）。
        let (done, running, idle) = dual_bar_cells(0, 0, 0, LOADING_BAR_CELLS);
        assert_eq!((done, running, idle), (0, 0, LOADING_BAR_CELLS));
    }

    /// 完了 0 のときでも稼働中 □ と「N active」が見えること＝本修正の目的。
    /// 従来は完了時の inc だけでバーが進み、パイプライン充填中は 0/len で止まった。
    /// その後プラグインが完了すると ■ も現れ、両領域が同時に見える。
    #[test]
    fn loading_bar_shows_running_region_even_at_zero_done() {
        let (term, screen) = ScreenTermLike::new(100);
        let mut m =
            ProgressManager::with_draw_target(ProgressDrawTarget::term_like(Box::new(term)));

        m.process(Message::LoadBegin { total: 10 });
        // 3 プラグインが fetch を開始（稼働中）。完了はまだ 0。
        for _ in 0..3 {
            m.process(Message::LoadPluginRunning);
        }
        // 稼働中の更新は inc を伴わないため steady_tick(120ms) の描画を待つ。
        std::thread::sleep(Duration::from_millis(180));

        let rendered = screen_rendered(&screen);
        assert!(
            rendered.contains('□'),
            "running region must show before any completion; got:\n{rendered}"
        );
        assert!(
            rendered.contains("0/10"),
            "pos/len at zero done; got:\n{rendered}"
        );
        assert!(
            rendered.contains("3 active"),
            "running count in annotation; got:\n{rendered}"
        );

        // 2 プラグイン完了 → ■(Done) と □(稼働中) が両方見える。
        m.process(Message::LoadPluginDone);
        m.process(Message::LoadPluginDone);
        std::thread::sleep(Duration::from_millis(180));

        let rendered = screen_rendered(&screen);
        assert!(rendered.contains('■'), "done region; got:\n{rendered}");
        assert!(rendered.contains('□'), "running region; got:\n{rendered}");
        assert!(
            rendered.contains("2/10"),
            "pos/len after completions; got:\n{rendered}"
        );

        // 後片付け: LoadDone -> MergeFinished で Loading 行は消える。
        m.process(Message::LoadDone);
        m.process(Message::MergeFinished {
            total: 2,
            merged: 1,
        });
        std::thread::sleep(Duration::from_millis(180));
        let rendered = screen_rendered(&screen);
        assert!(
            !rendered.contains("Loading"),
            "loading bar must be cleared after merge; got:\n{rendered}"
        );
    }
}

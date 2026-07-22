use console::style;
use hashbrown::HashMap;
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
    /// fetched 行を300ms経過後に空欄化する。世代で最新のタイマーだけ有効。
    FetchDoneIdle {
        idle_gen: u64,
    },
    /// フラグなし実行でキャッシュが無くロードできなかった（未インストール）。
    PluginNotInstalled(Arc<str>),
    /// `-u` でリモートの rev が変化し、実際に更新されたプラグイン（表示名）。
    PluginUpdated(Arc<str>),
    /// `-i` で未インストールから新規 fetch されたプラグイン（表示名）。
    PluginInstalled(Arc<str>),
    /// `dotgit=true` なのに snapshot に `.git` が無いプラグイン（表示名）。
    PluginDotgitMissing(Arc<str>),
    MergeFinished {
        total: usize,
        merged: usize,
    },
    DetectLockFile(PathBuf),
    /// GitHub GraphQL バッチ rev 解決が失敗し、per-repo 解決へフォールバックした。
    GraphQLBatchFailed {
        reason: String,
    },
    /// GraphQL rev 解決の進捗（resolved/total リポジトリ）。resolved>=total で完了。
    GraphQLResolveProgress {
        resolved: usize,
        total: usize,
    },
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

const GRAPHQL_PROGRESS_ID: &str = "graphql_resolve";
const CACHE_FETCH_PROGRESS_ID: &str = "cache_fetch_progress";
const CACHE_FETCH_STAGE_ID: &str = "cache_fetch_stage";
const CACHE_FETCH_DONE_ID: &str = "cache_fetch_done";

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

/// プラグイン名一覧の本体行を組み立てる。先頭3件を個別に20字 truncate して ` · ` で結合し、
/// 超過分は ` …` で省略する。未インストール/更新/新規インストールの各サマリーで共通利用。
fn ellipsis_names(names: &[Arc<str>]) -> String {
    const NAME_DISPLAY_LIMIT: usize = 20;
    let n = names.len();
    let shown: Vec<String> = names
        .iter()
        .take(3)
        .map(|s| truncate(s, NAME_DISPLAY_LIMIT))
        .collect();
    let mut body = shown.join(" · ");
    if n > shown.len() {
        body.push_str(" …");
    }
    body
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
    /// Fetching / Initializing 行（`progress_bars[CACHE_FETCH_STAGE_ID]`）。
    FetchStage,
    /// fetched 完了ノート行（`progress_bars[CACHE_FETCH_DONE_ID]`）。
    FetchDone,
    /// Fetch object aggregate progress（`progress_bars[CACHE_FETCH_PROGRESS_ID]`）。
    FetchProgress,
    /// Updating 行（`self.updating_bar`）。
    Updating,
    /// GraphQL rev 解決進捗行（`progress_bars[GRAPHQL_PROGRESS_ID]`）。
    GraphQLResolve,
    /// ビルド行（`progress_bars[id]`）。
    Building(Arc<String>),
}

struct ProgressManager {
    multipb: MultiProgress,
    pb_style: ProgressStyle,
    pb_style_spinner_mid: ProgressStyle,
    pb_style_spinner_last: ProgressStyle,
    pb_style_blank_mid: ProgressStyle,
    pb_style_blank_last: ProgressStyle,
    pb_style_fetch_note_child_mid: ProgressStyle,
    pb_style_fetch_note_child_last: ProgressStyle,
    pb_style_fetch_bar_mid: ProgressStyle,
    pb_style_fetch_bar_last: ProgressStyle,
    pb_style_summary: ProgressStyle,

    // State
    progress_bars: HashMap<String, BarState>,
    installskipped_count: usize,
    yankfile_count: usize,
    not_installed: Vec<Arc<str>>,
    /// `-u` で実際に rev が変わった（更新された）プラグインの表示名。
    updated_plugins: Vec<Arc<str>>,
    /// `-i` で新規 fetch されたプラグインの表示名。
    installed_plugins: Vec<Arc<str>>,
    /// `dotgit=true` なのに `.git` がなく、pack へ copy できないプラグインの表示名。
    dotgit_missing: Vec<Arc<str>>,
    cachefetching_oids: HashMap<String, (usize, usize)>,
    cache_updating_fetching: HashMap<String, ()>,
    cache_updating_current: Option<String>,
    updating_bar: Option<BarState>,
    /// Fetching 進行中の URL 集合（Updating 行と同じ並行追跡パターン）。
    cache_fetching: HashMap<String, ()>,
    /// Fetching 行に現在表示中の URL。
    cache_fetching_current: Option<String>,
    /// fetched 行に表示中の URL（最後に完了した1つ）。
    cache_fetch_done_url: Option<String>,
    /// Fetching 行が空欄（進行中なし）かどうか。
    fetch_stage_is_blank: bool,
    /// fetched 行が空欄（300ms 経過）かどうか。
    fetch_done_is_blank: bool,
    /// fetched 行の空欄化タイマーの世代。新しい Fetching:done ごとに進め、
    /// 世代が一致しない遅延メッセージは無視する（キャンセル相当）。
    fetch_done_idle_gen: u64,
    /// レベル1子バーの追加順。最後の子の罫線を `└─` に切り替えるために使う。
    child_order: Vec<ChildKey>,
    config_files: Vec<Arc<Path>>,
    osc94: Option<OSC94>,
    /// Loading バーの「稼働中」計数。LoadBegin より前に EARLY が始まるストリーミング経路でも
    /// 正しく引き継ぐため、バーの有無にかかわらず `loading_running_count` を正とする。
    loading_running: Option<Arc<AtomicUsize>>,
    loading_running_count: usize,
    /// fetched 行の300ms空欄化タイマー用の sender。
    /// 本番は `init` が注入、テストは None（タイマー不起動、`FetchDoneIdle` を直接
    /// `process` に送って検証）。グローバル LOGGER に依存しないことで単体テストを可能にする。
    idle_tx: Option<mpsc::UnboundedSender<Message>>,
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

fn loading_bar_width(running: usize) -> usize {
    let term_width = console::Term::stderr().size().1 as usize;
    loading_bar_width_for(term_width, running)
}

fn loading_bar_width_for(term_width: usize, running: usize) -> usize {
    let active_width = running.to_string().width() + " active".width();
    // Matches loading_style(): "{spinner} {prefix} [{elapsed}] [{dualbar}] {pos}/{len} {active}".
    let fixed_width = 40 + active_width;
    term_width.saturating_sub(fixed_width).max(10)
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
        let width = loading_bar_width(running as usize);
        let (done_cells, running_cells, idle_cells) = dual_bar_cells(done, running, len, width);
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
    ProgressStyle::with_template(
        "{spinner} {prefix:.blue.bold} [{elapsed_precise}] [{dualbar}] {pos:>7}/{len:7} {active}",
    )
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
    /// 検証用: stderr 描画、タイマー sender 無し。
    #[cfg(test)]
    fn new() -> Self {
        Self::build(ProgressDrawTarget::stderr(), None)
    }

    /// 描画先を注入可能にしたコンストラクタ（検証用）。本番は `init` が
    /// `build(stderr, Some(tx))` を呼ぶ。テストはタイマー sender 無し。
    #[cfg(test)]
    fn with_draw_target(draw_target: ProgressDrawTarget) -> Self {
        Self::build(draw_target, None)
    }

    fn build(
        draw_target: ProgressDrawTarget,
        idle_tx: Option<mpsc::UnboundedSender<Message>>,
    ) -> Self {
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
        let pb_style_fetch_bar_mid = ProgressStyle::with_template(
            "{spinner}  ├─ {prefix:.blue.bold} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos:>7}/{len:7}",
        )
        .unwrap()
        .progress_chars("■□ ")
        .tick_strings(&["◒", "◐", "◓", "◑", " "]);
        let pb_style_fetch_bar_last = ProgressStyle::with_template(
            "{spinner}  └─ {prefix:.blue.bold} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos:>7}/{len:7}",
        )
        .unwrap()
        .progress_chars("■□ ")
        .tick_strings(&["◒", "◐", "◓", "◑", " "]);
        // 空欄行（Fetching が進行中なし／fetched が300ms経過）。罫線のみ、内容は描かない。
        // 中間は │、最後は空白。レベル1バーと fetched(レベル2)行で共用。
        // tick_strings は2要素（最終要素は indicatif の完了状態用に予約されるため、
        // アニメーション要素を1要素だけにするとゼロ除算になる）。
        let pb_style_blank_mid = ProgressStyle::with_template("{spinner}  │")
            .unwrap()
            .tick_strings(&[" ", " "]);
        let pb_style_blank_last = ProgressStyle::with_template("{spinner}   ")
            .unwrap()
            .tick_strings(&[" ", " "]);
        // fetched 行は Fetching の子（レベル2）。横線(─)は引かず、Fetching の縦線を継ぐか空白。
        // Fetching が中間(├─)なら │、Fetching が最後(└─)なら空白。いずれも Fetching より1段階右。
        let pb_style_fetch_note_child_mid =
            ProgressStyle::with_template("{spinner}  │   {prefix:.dim} {wide_msg}")
                .unwrap()
                .tick_strings(&[" ", " "]);
        let pb_style_fetch_note_child_last =
            ProgressStyle::with_template("{spinner}      {prefix:.dim} {wide_msg}")
                .unwrap()
                .tick_strings(&[" ", " "]);
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
            pb_style_blank_mid,
            pb_style_blank_last,
            pb_style_fetch_note_child_mid,
            pb_style_fetch_note_child_last,
            pb_style_fetch_bar_mid,
            pb_style_fetch_bar_last,
            pb_style_summary,
            progress_bars: HashMap::from([("config_files".to_string(), barstate)]),
            installskipped_count: 0,
            yankfile_count: 0,
            not_installed: Vec::new(),
            updated_plugins: Vec::new(),
            installed_plugins: Vec::new(),
            dotgit_missing: Vec::new(),
            cachefetching_oids: HashMap::new(),
            cache_updating_fetching: HashMap::new(),
            cache_updating_current: None,
            updating_bar: None,
            cache_fetching: HashMap::new(),
            cache_fetching_current: None,
            cache_fetch_done_url: None,
            fetch_stage_is_blank: false,
            fetch_done_is_blank: false,
            fetch_done_idle_gen: 0,
            child_order: Vec::new(),
            config_files: Vec::new(),
            osc94: None,
            loading_running: None,
            loading_running_count: 0,
            idle_tx,
        }
    }

    /// 子キーから対応するアクティブな BarState を引く。
    /// バーが既に除去されている（Map/Option に無い）場合は None。
    fn child_bar(&self, key: &ChildKey) -> Option<&BarState> {
        match key {
            ChildKey::FetchStage => self.progress_bars.get(CACHE_FETCH_STAGE_ID),
            ChildKey::FetchDone => self.progress_bars.get(CACHE_FETCH_DONE_ID),
            ChildKey::FetchProgress => self.progress_bars.get(CACHE_FETCH_PROGRESS_ID),
            ChildKey::Updating => self.updating_bar.as_ref(),
            ChildKey::GraphQLResolve => self.progress_bars.get(GRAPHQL_PROGRESS_ID),
            ChildKey::Building(id) => self.progress_bars.get(id.as_str()),
        }
    }

    /// 子バーの表示カテゴリ優先順（＝物理 insert 順）。
    /// `child_order` の push 順はバーの再生成時に物理 insert 順とずれうるため、
    /// 罫線（├─/└─）はこの優先順で決める。Building 同士は元の追加順を維持（sort は安定）。
    fn category_rank(key: &ChildKey) -> u8 {
        match key {
            ChildKey::GraphQLResolve => 0,
            ChildKey::Updating => 1,
            ChildKey::FetchStage => 2,
            ChildKey::FetchDone => 3,
            ChildKey::FetchProgress => 4,
            ChildKey::Building(_) => 5,
        }
    }

    /// 子バーの追加/除去後に呼び、表示順に従い最後の子に └─・それ以外に ├─ を当て直す。
    /// `child_order` の push 順は物理 insert 順とずれうるため、`category_rank` で整列して
    /// から罫線を決める（物理 insert も同じ優先順を保証している）。
    fn refresh_connectors(&self) {
        let mut ordered: Vec<&ChildKey> = self.child_order.iter().collect();
        ordered.sort_by_key(|k| Self::category_rank(k));
        // fetched は Fetching の子（レベル2）。レベル1の最後は FetchDone を除いて決める。
        let level1_last = ordered
            .iter()
            .copied()
            .rfind(|k| !matches!(k, ChildKey::FetchDone));
        let fetch_is_last = level1_last == Some(&ChildKey::FetchStage);
        for key in ordered {
            let Some(bs) = self.child_bar(key) else {
                continue;
            };
            let is_level1_last = Some(key) == level1_last;
            let style = match key {
                ChildKey::FetchStage if self.fetch_stage_is_blank && is_level1_last => {
                    self.pb_style_blank_last.clone()
                }
                ChildKey::FetchStage if self.fetch_stage_is_blank => {
                    self.pb_style_blank_mid.clone()
                }
                ChildKey::FetchStage if is_level1_last => self.pb_style_spinner_last.clone(),
                ChildKey::FetchStage => self.pb_style_spinner_mid.clone(),
                ChildKey::FetchDone => {
                    // fetched は Fetching の子。Fetching の最終性で縦線(│)を継ぐか空白か。
                    if self.fetch_done_is_blank {
                        if fetch_is_last {
                            self.pb_style_blank_last.clone()
                        } else {
                            self.pb_style_blank_mid.clone()
                        }
                    } else if fetch_is_last {
                        self.pb_style_fetch_note_child_last.clone()
                    } else {
                        self.pb_style_fetch_note_child_mid.clone()
                    }
                }
                ChildKey::GraphQLResolve if is_level1_last => self.pb_style_fetch_bar_last.clone(),
                ChildKey::GraphQLResolve => self.pb_style_fetch_bar_mid.clone(),
                ChildKey::FetchProgress if is_level1_last => self.pb_style_fetch_bar_last.clone(),
                ChildKey::FetchProgress => self.pb_style_fetch_bar_mid.clone(),
                _ if is_level1_last => self.pb_style_spinner_last.clone(),
                _ => self.pb_style_spinner_mid.clone(),
            };
            bs.bar.set_style(style);
            // set_style 自体は即時描画しない。steady_tick していないバー（note/空欄系）
            // にも罫線切替を即時反映するため、ここで明示的に tick する。
            bs.bar.tick();
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

    /// Objects 行（CACHE_FETCH_PROGRESS_ID）を表示順末尾に挿入する。
    /// 表示順 [Updating]→[Fetching]→[fetched]→[Objects] の最後。
    fn add_fetch_progress_bar(&self, pb: ProgressBar) -> ProgressBar {
        if let Some(done) = self.progress_bars.get(CACHE_FETCH_DONE_ID) {
            self.multipb.insert_after(&done.bar, pb)
        } else if let Some(stage) = self.progress_bars.get(CACHE_FETCH_STAGE_ID) {
            self.multipb.insert_after(&stage.bar, pb)
        } else if let Some(updating) = self.updating_bar.as_ref() {
            self.multipb.insert_after(&updating.bar, pb)
        } else {
            self.multipb.add(pb)
        }
    }

    /// GraphQL rev 解決進捗行（GRAPHQL_PROGRESS_ID）を挿入する。
    /// 順序上 Updating/Fetching/Objects の前（preresolved が最初）。
    fn add_graphql_bar(&self, pb: ProgressBar) -> ProgressBar {
        if let Some(updating) = self.updating_bar.as_ref() {
            self.multipb.insert_before(&updating.bar, pb)
        } else if let Some(stage) = self.progress_bars.get(CACHE_FETCH_STAGE_ID) {
            self.multipb.insert_before(&stage.bar, pb)
        } else if let Some(progress) = self.progress_bars.get(CACHE_FETCH_PROGRESS_ID) {
            self.multipb.insert_before(&progress.bar, pb)
        } else {
            self.multipb.add(pb)
        }
    }

    /// Fetching 行（CACHE_FETCH_STAGE_ID）を挿入する。
    /// 順序上 Updating の直後、fetched/Objects の前。
    fn add_fetch_stage_bar(&self, pb: ProgressBar) -> ProgressBar {
        if let Some(updating) = self.updating_bar.as_ref() {
            self.multipb.insert_after(&updating.bar, pb)
        } else if let Some(done) = self.progress_bars.get(CACHE_FETCH_DONE_ID) {
            self.multipb.insert_before(&done.bar, pb)
        } else if let Some(progress) = self.progress_bars.get(CACHE_FETCH_PROGRESS_ID) {
            self.multipb.insert_before(&progress.bar, pb)
        } else {
            self.multipb.add(pb)
        }
    }

    /// fetched 行（CACHE_FETCH_DONE_ID）を挿入する。
    /// 順序上 Fetching の直後、Objects の前。
    fn add_fetch_done_bar(&self, pb: ProgressBar) -> ProgressBar {
        if let Some(stage) = self.progress_bars.get(CACHE_FETCH_STAGE_ID) {
            self.multipb.insert_after(&stage.bar, pb)
        } else if let Some(updating) = self.updating_bar.as_ref() {
            self.multipb.insert_after(&updating.bar, pb)
        } else if let Some(progress) = self.progress_bars.get(CACHE_FETCH_PROGRESS_ID) {
            self.multipb.insert_before(&progress.bar, pb)
        } else {
            self.multipb.add(pb)
        }
    }

    fn ensure_fetch_stage(&mut self, stage: &'static str, url: &str) {
        self.cache_fetching.insert(url.to_string(), ());
        let existed = self.progress_bars.contains_key(CACHE_FETCH_STAGE_ID);
        if !existed {
            let pb = ProgressBar::new_spinner()
                .with_style(self.pb_style_spinner_mid.clone())
                .with_prefix("Fetching");
            pb.enable_steady_tick(Duration::from_millis(100));
            let pb = self.add_fetch_stage_bar(pb);
            self.progress_bars
                .insert(CACHE_FETCH_STAGE_ID.to_string(), BarState::new(pb));
            self.register_child(ChildKey::FetchStage);
        }
        // 空欄状態（進行中なしだった）からアクティブに復帰。
        self.fetch_stage_is_blank = false;
        let bar_state = self.progress_bars.get_mut(CACHE_FETCH_STAGE_ID).unwrap();
        bar_state.bar.set_prefix(stage);
        // 他の URL が表示中でなければ更新（Updating 行と同じ判断: 上書きしない）。
        if self.cache_fetching_current.is_none()
            || self.cache_fetching_current.as_deref() == Some(url)
        {
            bar_state.set_message_if_changed(url.to_string());
            self.cache_fetching_current = Some(url.to_string());
        }
        self.refresh_connectors();
    }

    /// `Fetching:done` 到着時の処理。fetched 専用バー（CACHE_FETCH_DONE_ID）を
    /// 独立ライフサイクルで扱い、Fetching 行とは別行に表示する。
    /// 当該 URL を進行中集合から外し、進行中が尽きたら Fetching 行を**空欄**にする
    /// （行は消さず保つ）。fetched 行は300ms後に空欄化される（`FetchDoneIdle`）。
    fn mark_fetch_done(&mut self, url: &str) {
        let existed = self.progress_bars.contains_key(CACHE_FETCH_DONE_ID);
        if !existed {
            let pb = ProgressBar::new_spinner()
                .with_style(self.pb_style_fetch_note_child_mid.clone())
                .with_prefix("fetched");
            let pb = self.add_fetch_done_bar(pb);
            self.progress_bars
                .insert(CACHE_FETCH_DONE_ID.to_string(), BarState::new(pb));
            self.register_child(ChildKey::FetchDone);
        }
        let bar_state = self.progress_bars.get_mut(CACHE_FETCH_DONE_ID).unwrap();
        bar_state.set_message_if_changed(format!("{}", style(url).dim().italic()));
        self.cache_fetch_done_url = Some(url.to_string());
        // fetched 行をアクティブ表示（300ms後に空欄化タイマーが発火）。
        self.fetch_done_is_blank = false;
        // 300ms 後に fetched 行を空欄にする。世代を進め、最新のタイマーだけ有効にする。
        self.fetch_done_idle_gen = self.fetch_done_idle_gen.wrapping_add(1);
        let idle_gen = self.fetch_done_idle_gen;
        if let Some(tx) = self.idle_tx.clone() {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = tx.send(Message::FetchDoneIdle { idle_gen });
            });
        }

        // Fetching 行の完了処理: 当該 URL を進行中集合から外す。
        self.cache_fetching.remove(url);
        if self.cache_fetching.is_empty() {
            // 進行中が無くなったら Fetching 行を空欄に（消さず行を保つ）。
            // last_message をリセットし、再 Fetching 時に set_message_if_changed が確実に送るようにする。
            self.fetch_stage_is_blank = true;
            if let Some(bs) = self.progress_bars.get_mut(CACHE_FETCH_STAGE_ID) {
                bs.last_message = None;
            }
            self.cache_fetching_current = None;
        } else if self.cache_fetching_current.as_deref() == Some(url) {
            // 表示中 URL が完了したら残存のいずれかに切替（Updating 行と同じ）。
            let next = self.cache_fetching.keys().next().cloned();
            self.cache_fetching_current = next.clone();
            if let Some(bs) = self.progress_bars.get_mut(CACHE_FETCH_STAGE_ID) {
                bs.set_message_if_changed(next.unwrap_or_default());
            }
        }
        self.refresh_connectors();
    }

    fn clear_fetch_stage(&mut self) {
        if let Some(pb) = self.progress_bars.remove(CACHE_FETCH_STAGE_ID) {
            pb.bar.finish_and_clear();
            self.unregister_child(&ChildKey::FetchStage);
        }
        if let Some(pb) = self.progress_bars.remove(CACHE_FETCH_DONE_ID) {
            pb.bar.finish_and_clear();
            self.unregister_child(&ChildKey::FetchDone);
        }
        self.cache_fetching.clear();
        self.cache_fetching_current = None;
        self.cache_fetch_done_url = None;
        self.fetch_stage_is_blank = false;
        self.fetch_done_is_blank = false;
        // 保留中の 300ms タイマーを無効化（世代を進めて不一致にする）。
        self.fetch_done_idle_gen = self.fetch_done_idle_gen.wrapping_add(1);
    }

    /// ヘッダー行と名前一覧（`ellipsis_names`）から2行ブロックを組み立てて印字する。
    /// `warn_not_installed` / `print_plugin_list_block` の共通の描画本体。
    fn print_name_block(&self, header: String, names: &[Arc<str>]) {
        let body = ellipsis_names(names);
        let block = if body.is_empty() {
            header
        } else {
            format!("{header}\n    {}", style(body).dim())
        };
        self.multipb.println(block).unwrap();
    }

    /// フラグなし実行でキャッシュが無くロードできなかったプラグインの警告を印字。
    /// 各 name は logid のような共通 truncate ではなく、**個別に** 20字超のときだけ
    /// truncate する。これにより複数プラグインを並べても何が未インストールか判読できる。
    fn warn_not_installed(&self) {
        let header = format!(
            "{} {} plugins not installed (run with -i to install)",
            style("⚠").yellow().bold(),
            self.not_installed.len()
        );
        self.print_name_block(header, &self.not_installed);
    }

    /// `dotgit=true` なのに snapshot に `.git` が無く、pack へ copy できない警告。
    fn warn_dotgit_missing(&self) {
        let header = format!(
            "{} {} dotgit plugins missing `.git` (run with -u to refresh)",
            style("⚠").yellow().bold(),
            self.dotgit_missing.len()
        );
        self.print_name_block(header, &self.dotgit_missing);
    }

    /// 更新/新規インストールされたプラグインのサマリーブロックを印字。
    /// `warn_not_installed` と同じ体裁（個別20字 truncate・先頭3件・超過は ` …`）。
    fn print_plugin_list_block(&self, label: &str, names: &[Arc<str>]) {
        let header = format!(
            "{} {} plugins",
            summary_prefix(label, true),
            style(names.len()).green().bold(),
        );
        self.print_name_block(header, names);
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
                // MultiProgress::println は orphan 行として、後続バーの描画領域より上に
                // 永続出力する。各行を独立して渡し、行末カーソル位置も確実に同期する。
                for line in display.render_lines() {
                    self.multipb.println(style(line).dim().to_string()).unwrap();
                }
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
                // loading バー消去後（アクティブバーが無い状態）で印字する。LoadDone では
                // "loading" バーを残したまま println するため indicatif の再描画で上書きされて
                // 出力に残らない。ここで Loaded と同タイミングに出すことで永続化する。
                if !self.not_installed.is_empty() {
                    self.warn_not_installed();
                }
                if !self.updated_plugins.is_empty() {
                    self.print_plugin_list_block("Updated", &self.updated_plugins);
                }
                if !self.installed_plugins.is_empty() {
                    self.print_plugin_list_block("Installed", &self.installed_plugins);
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
                if r#type == "Fetching:done" {
                    self.mark_fetch_done(url.as_ref());
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
                let existed = self.progress_bars.contains_key(CACHE_FETCH_PROGRESS_ID);
                if !existed {
                    let pb = ProgressBar::new(0)
                        .with_style(self.pb_style_fetch_bar_last.clone())
                        .with_prefix("Objects");
                    pb.enable_steady_tick(Duration::from_millis(100));
                    let pb = self.add_fetch_progress_bar(pb);
                    self.progress_bars
                        .insert(CACHE_FETCH_PROGRESS_ID.to_string(), BarState::new(pb));
                    self.register_child(ChildKey::FetchProgress);
                }
                let pb = self.progress_bars.get_mut(CACHE_FETCH_PROGRESS_ID).unwrap();
                let (prev_total, prev_received) = self.cachefetching_oids.entry(id).or_default();
                pb.bar
                    .inc_length(total_objs_count.saturating_sub(*prev_total) as u64);
                pb.bar
                    .inc(received_objs_count.saturating_sub(*prev_received) as u64);
                *prev_total = total_objs_count;
                *prev_received = received_objs_count;
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
                let running = Arc::new(AtomicUsize::new(self.loading_running_count));
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
                self.loading_running_count = self.loading_running_count.saturating_add(1);
                if let Some(running) = self.loading_running.as_ref() {
                    running.store(self.loading_running_count, Ordering::Relaxed);
                }
            }
            Message::LoadPluginRunningDone => {
                self.loading_running_count = self.loading_running_count.saturating_sub(1);
                if let Some(running) = self.loading_running.as_ref() {
                    running.store(self.loading_running_count, Ordering::Relaxed);
                }
            }
            Message::LoadPluginDone => {
                if let Some(state) = self.progress_bars.get_mut("loading") {
                    state.bar.inc(1);
                }
            }
            Message::FetchDoneIdle { idle_gen } => {
                // 世代が一致しなければ古いタイマーとして無視。
                if idle_gen != self.fetch_done_idle_gen {
                    return;
                }
                // fetched 行を空欄に。last_message をリセットし、次回の fetched 表示で確実に送る。
                if let Some(bs) = self.progress_bars.get_mut(CACHE_FETCH_DONE_ID) {
                    bs.last_message = None;
                }
                self.fetch_done_is_blank = true;
                self.refresh_connectors();
            }
            Message::LoadDone => {
                self.clear_fetch_stage();
                drop(self.osc94.take());
                // これ以降稼働中計数は更新されない（バーは MergeFinished まで残留表示）。
                self.loading_running = None;
                self.loading_running_count = 0;
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
                // not_installed/Updated/Installed ブロックの印字は MergeFinished に移動。
                // ここでは "loading" バーを表示領域に残すため、バー生存中の println となり
                // indicatif の再描画で上書きされて出力に残らない（Loaded と同じく loading
                // バー消去後に印字する必要がある）。
            }
            Message::PluginNotInstalled(id) => {
                self.not_installed.push(id);
            }
            Message::PluginUpdated(id) => {
                self.updated_plugins.push(id);
            }
            Message::PluginInstalled(id) => {
                self.installed_plugins.push(id);
            }
            Message::PluginDotgitMissing(id) => {
                self.dotgit_missing.push(id);
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
            Message::GraphQLBatchFailed { reason } => {
                self.multipb
                    .println(format!(
                        "{} batch failed ({}); falling back to per-repo resolution",
                        summary_prefix("GraphQL", false),
                        reason
                    ))
                    .unwrap();
            }
            Message::GraphQLResolveProgress { resolved, total } => {
                if total == 0 {
                    return;
                }
                if !self.progress_bars.contains_key(GRAPHQL_PROGRESS_ID) {
                    let pb = ProgressBar::new(total as u64)
                        .with_style(self.pb_style_fetch_bar_last.clone())
                        .with_prefix("Resolving");
                    let pb = self.add_graphql_bar(pb);
                    self.progress_bars
                        .insert(GRAPHQL_PROGRESS_ID.to_string(), BarState::new(pb));
                    self.register_child(ChildKey::GraphQLResolve);
                }
                let pb = self.progress_bars.get_mut(GRAPHQL_PROGRESS_ID).unwrap();
                pb.bar.set_length(total as u64);
                pb.bar.set_position(resolved as u64);
                if resolved >= total {
                    if let Some(bs) = self.progress_bars.remove(GRAPHQL_PROGRESS_ID) {
                        // GraphQL 解決は後続の Loading に先行する一時進捗であり、完了行を
                        // 永続化しない。`finish_with_message` は完了済みバーを MultiProgress
                        // 上に残すため、次のフェーズの罫線・行再描画と競合する。
                        bs.bar.finish_and_clear();
                    }
                    self.child_order.retain(|k| k != &ChildKey::GraphQLResolve);
                    self.refresh_connectors();
                }
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
                if !self.dotgit_missing.is_empty() {
                    self.warn_dotgit_missing();
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
    // FetchDoneIdle（300ms遅延）用にメイン channel とは別の channel を持つ。
    // メインと同じ sender を manager に持たせると、close() が sender を drop しても
    // manager 内の clone が生きて rx が閉じず、受信ループが抜けず終了しなくなる。
    let (idle_tx, mut idle_rx) = mpsc::unbounded_channel::<Message>();
    tokio::spawn(async move {
        let mut manager = ProgressManager::build(ProgressDrawTarget::stderr(), Some(idle_tx));
        loop {
            // メイン channel が閉じたら（close 呼出）即座に抜ける。
            // idle channel は manager が idle_tx を保持するため自力では閉じない。
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(msg) => manager.process(msg),
                    None => break,
                },
                msg = idle_rx.recv() => {
                    if let Some(msg) = msg {
                        manager.process(msg);
                    }
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
                        // indicatif は行を端末幅まで埋めた後、改行文字を使わずに次の
                        // 描画行へ移動する。実端末と同様に幅を超えた最初の文字で折り返す。
                        if s.col >= self.width as usize {
                            s.row += 1;
                            s.col = 0;
                            while s.rows.len() <= s.row {
                                s.rows.push(String::new());
                            }
                        }
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

    /// GraphQL の解決進捗は一時表示であり、完了後の最終画面には残らない。
    /// `finish_with_message` は MultiProgress 上で完了行を永続化するため、完了時には
    /// `finish_and_clear` で描画領域からも取り除く必要がある。
    #[test]
    fn graphql_resolve_progress_clears_after_completion() {
        let (term, screen) = ScreenTermLike::new(120);
        let mut m =
            ProgressManager::with_draw_target(ProgressDrawTarget::term_like(Box::new(term)));

        m.process(Message::GraphQLResolveProgress {
            resolved: 0,
            total: 124,
        });
        m.process(Message::GraphQLResolveProgress {
            resolved: 124,
            total: 124,
        });

        let rendered = screen_rendered(&screen);
        assert!(
            !rendered.contains("Resolving"),
            "completed GraphQL progress must be cleared; got:\n{rendered}"
        );
    }

    /// Resolving を消去した直後に後続フェーズが描画されても、先に println した
    /// Config サマリーの最終行が進捗の再描画で上書きされない。
    #[test]
    fn config_summary_survives_resolving_clear_before_following_progress() {
        let (term, screen) = ScreenTermLike::new(120);
        let mut m =
            ProgressManager::with_draw_target(ProgressDrawTarget::term_like(Box::new(term)));

        for name in ["alpha", "beta", "gamma"] {
            m.process(Message::ConfigFound(PathBuf::from(format!(
                "/tmp/rsplug/{name}.toml"
            ))));
        }
        m.process(Message::ConfigWalkFinish);
        m.process(Message::GraphQLResolveProgress {
            resolved: 27,
            total: 124,
        });
        m.process(Message::GraphQLResolveProgress {
            resolved: 124,
            total: 124,
        });
        m.process(Message::Cache(
            "Updating",
            Arc::from("https://example.test/plugin"),
        ));
        m.process(Message::LoadBegin { total: 128 });

        std::thread::sleep(Duration::from_millis(160));
        let rendered = screen_rendered(&screen);
        let rendered_lines = rendered.lines().map(str::trim_end).collect::<Vec<_>>();
        assert!(
            rendered_lines.contains(&"        alpha beta gamma"),
            "config summary's final line must survive Resolving clear; got:\n{rendered}"
        );
        assert!(
            !rendered.contains("Resolving"),
            "completed Resolving bar must not remain; got:\n{rendered}"
        );
    }

    /// 永続表示する Config サマリーは、その後に開始する進捗バーの描画領域より上に
    /// 独立した行として残る。複数行の文字列を `MultiProgress::println` に直接渡すと、
    /// 行末カーソル位置が同期されず、最初の進捗バーがサマリーを上書きしてしまう。
    #[test]
    fn config_summary_remains_on_separate_lines_above_active_progress() {
        let (term, screen) = ScreenTermLike::new(120);
        let mut m =
            ProgressManager::with_draw_target(ProgressDrawTarget::term_like(Box::new(term)));

        for name in ["alpha", "beta", "gamma"] {
            m.process(Message::ConfigFound(PathBuf::from(format!(
                "/tmp/rsplug/{name}.toml"
            ))));
        }
        m.process(Message::ConfigWalkFinish);
        let rendered_after_config = screen_rendered(&screen);
        assert!(
            rendered_after_config.contains("Config 3 files"),
            "config summary must render before progress starts; got:\n{rendered_after_config}"
        );
        m.process(Message::GraphQLResolveProgress {
            resolved: 27,
            total: 124,
        });
        m.process(Message::Cache(
            "Updating",
            Arc::from("https://example.test/plugin"),
        ));
        m.process(Message::LoadBegin { total: 128 });

        std::thread::sleep(Duration::from_millis(160));
        let rendered = screen_rendered(&screen);
        let rendered_lines = rendered.lines().map(str::trim_end).collect::<Vec<_>>();
        let config_lines = [
            "Config 3 files",
            "    /tmp/rsplug (3)",
            "        alpha beta gamma",
        ];
        for line in config_lines {
            assert!(
                rendered_lines.contains(&line),
                "config line must remain separate; missing {line:?} in:\n{rendered}"
            );
        }
        let config_last = rendered_lines
            .iter()
            .position(|line| *line == "        alpha beta gamma")
            .unwrap();
        let resolving = rendered_lines
            .iter()
            .position(|line| line.contains("Resolving"))
            .unwrap();
        assert!(
            config_last < resolving,
            "config summary must be above progress bars; got:\n{rendered}"
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
        m.process(Message::MergeFinished {
            total: 2,
            merged: 2,
        });

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
        m.process(Message::MergeFinished {
            total: 4,
            merged: 4,
        });

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

    #[test]
    fn warn_dotgit_missing_uses_refresh_hint() {
        let (term, screen) = ScreenTermLike::new(120);
        let mut m =
            ProgressManager::with_draw_target(ProgressDrawTarget::term_like(Box::new(term)));

        m.process(Message::PluginDotgitMissing(Arc::from("dotgit-plugin")));
        m.process(Message::InstallDone);

        let rendered = {
            let s = screen.lock().unwrap();
            s.rows
                .iter()
                .map(|r| console::strip_ansi_codes(r))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(
            rendered.contains("dotgit plugins missing `.git`"),
            "dotgit warning should explain the missing payload; got:\n{rendered}"
        );
        assert!(
            rendered.contains("run with -u to refresh"),
            "dotgit warning should point at -u; got:\n{rendered}"
        );
    }

    /// `-u` で実際に更新されたプラグインが、MergeFinished で `✓ Updated N plugins`
    /// ブロック（先頭3件・個別 truncate・超過 ` …`）として印字されることを検証する。
    /// LoadDone ではなく MergeFinished（loading バー消去後）で出すことで、 indicatif の
    /// バー再描画で上書きされず出力に永続化される。
    #[test]
    fn updated_block_lists_names_with_ellipsis() {
        let (term, screen) = ScreenTermLike::new(120);
        let mut m =
            ProgressManager::with_draw_target(ProgressDrawTarget::term_like(Box::new(term)));

        for s in ["aaaa", "bbbb", "cccc", "dddd"] {
            m.process(Message::PluginUpdated(Arc::from(s)));
        }
        m.process(Message::LoadDone);
        m.process(Message::MergeFinished {
            total: 4,
            merged: 4,
        });

        let rendered = screen_rendered(&screen);
        assert!(
            rendered.contains("Updated 4 plugins"),
            "header should report 4 updated; got:\n{rendered}"
        );
        assert!(
            rendered.contains(" …"),
            "remaining plugins should be elided; got:\n{rendered}"
        );
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

    /// child_order を表示順（category_rank）で整列したビュー。検証は整列後で行う
    /// （追加順はバーの再生成時に物理 insert 順とずれるため）。
    fn ordered_children(m: &ProgressManager) -> Vec<ChildKey> {
        let mut v: Vec<ChildKey> = m.child_order.to_vec();
        v.sort_by_key(ProgressManager::category_rank);
        v
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
        let (done, running, idle) = dual_bar_cells(65, 8, 130, 40);
        assert_eq!((done, running, idle), (20, 2, 18));

        // len=0 でも安全（分母を max(1) で護る）。
        let (done, running, idle) = dual_bar_cells(0, 0, 0, 40);
        assert_eq!((done, running, idle), (0, 0, 40));
    }

    #[test]
    fn loading_bar_width_uses_remaining_terminal_width() {
        assert_eq!(loading_bar_width_for(100, 3), 52);
        assert_eq!(loading_bar_width_for(20, 123), 10);
    }

    #[test]
    fn fetch_object_progress_aggregates_all_repos() {
        let (term, _screen) = ScreenTermLike::new(100);
        let mut m =
            ProgressManager::with_draw_target(ProgressDrawTarget::term_like(Box::new(term)));

        m.process(Message::CacheFetchObjectsProgress {
            id: "a".into(),
            total_objs_count: 100,
            received_objs_count: 40,
        });
        m.process(Message::CacheFetchObjectsProgress {
            id: "b".into(),
            total_objs_count: 50,
            received_objs_count: 10,
        });
        m.process(Message::CacheFetchObjectsProgress {
            id: "a".into(),
            total_objs_count: 120,
            received_objs_count: 60,
        });

        let pb = &m
            .progress_bars
            .get(CACHE_FETCH_PROGRESS_ID)
            .expect("object progress bar")
            .bar;
        assert_eq!(pb.length(), Some(170));
        assert_eq!(pb.position(), 70);
    }

    #[test]
    fn fetch_done_note_is_below_object_progress() {
        let (term, screen) = ScreenTermLike::new(100);
        let mut m =
            ProgressManager::with_draw_target(ProgressDrawTarget::term_like(Box::new(term)));

        let url = Arc::<str>::from("file:///x/a");
        m.process(Message::Cache("Updating", url.clone()));
        m.process(Message::Cache("Updating:done", url.clone()));
        m.process(Message::Cache("Fetching", url.clone()));
        m.process(Message::CacheFetchObjectsProgress {
            id: "a".into(),
            total_objs_count: 10,
            received_objs_count: 10,
        });
        std::thread::sleep(Duration::from_millis(160));

        let rendered = screen_rendered(&screen);
        let updating = rendered.find("Updating").expect("updating bar");
        let fetching = rendered.find("Fetching").expect("fetching bar");
        let objects = rendered.find("Objects").expect("objects bar");
        assert!(
            updating < fetching,
            "fetching should follow updating; got:\n{rendered}"
        );
        assert!(
            fetching < objects,
            "objects should follow fetching while fetch is running; got:\n{rendered}"
        );

        m.process(Message::Cache("Fetching:done", url));
        std::thread::sleep(Duration::from_millis(160));

        let rendered = screen_rendered(&screen);
        assert!(
            rendered.contains("fetched"),
            "done note missing; got:\n{rendered}"
        );
        let updating = rendered.find("Updating").expect("updating bar");
        let fetched = rendered.find("fetched").expect("done note");
        let objects = rendered.find("Objects").expect("objects bar");
        assert!(
            updating < fetched && fetched < objects,
            "done note should keep the fetching row position above object progress; got:\n{rendered}"
        );
        assert!(
            rendered.contains("file:///x/a"),
            "done url; got:\n{rendered}"
        );
    }

    /// Fetching 行と fetched 行が別バー（別行）として存在すること＝本修正（2バー分離）の中核。
    /// 従来は1バーを共用して prefix を "Fetching"⇔"fetched" で切り替えたため、
    /// 並行 fetch で同一行が入れ替わって見えた。
    #[test]
    fn fetching_and_fetched_are_separate_rows() {
        let (term, _screen) = ScreenTermLike::new(100);
        let mut m =
            ProgressManager::with_draw_target(ProgressDrawTarget::term_like(Box::new(term)));

        let a = Arc::<str>::from("file:///x/a");
        let b = Arc::<str>::from("file:///x/b");
        // 2 URL を並行 fetch 開始（いずれも進行中）。
        m.process(Message::Cache("Fetching", a.clone()));
        m.process(Message::Cache("Fetching", b.clone()));
        // 片方だけ完了 → fetched 行が出る。もう片方はまだ Fetching。
        m.process(Message::Cache("Fetching:done", a));

        // Fetching（b 進行中）と fetched（a）が別バーとして両方存在。
        assert!(
            m.progress_bars.contains_key(CACHE_FETCH_STAGE_ID),
            "Fetching bar must exist while b is inflight"
        );
        assert!(
            m.progress_bars.contains_key(CACHE_FETCH_DONE_ID),
            "fetched bar must exist for completed a"
        );
        // 表示順で FetchStage が FetchDone より前（Fetching 上、fetched 下）。
        assert_eq!(
            ordered_children(&m),
            vec![ChildKey::FetchStage, ChildKey::FetchDone],
            "Fetching and fetched must be distinct bars in order"
        );
    }

    /// 4バーが存在する状態で表示順 [Updating]→[Fetching]→[fetched]→[Objects] を検証。
    #[test]
    fn fetching_row_order_with_updating_and_objects() {
        let (term, _screen) = ScreenTermLike::new(100);
        let mut m =
            ProgressManager::with_draw_target(ProgressDrawTarget::term_like(Box::new(term)));

        let a = Arc::<str>::from("file:///x/a");
        let b = Arc::<str>::from("file:///x/b");
        m.process(Message::Cache("Updating", a.clone()));
        m.process(Message::Cache("Fetching", a.clone()));
        m.process(Message::Cache("Fetching", b.clone()));
        m.process(Message::Cache("Fetching:done", a.clone()));
        m.process(Message::CacheFetchObjectsProgress {
            id: "a".into(),
            total_objs_count: 10,
            received_objs_count: 5,
        });

        // 表示順は category_rank 整列で [Updating, FetchStage, FetchDone, FetchProgress]。
        assert_eq!(
            ordered_children(&m),
            vec![
                ChildKey::Updating,
                ChildKey::FetchStage,
                ChildKey::FetchDone,
                ChildKey::FetchProgress,
            ],
            "display order must be Updating<Fetching<fetched<Objects"
        );
    }

    /// 2 URL を交互に Fetching しても表示中 URL（最初）が上書きされないこと。
    /// Updating 行と同じ並行追跡パターン（current は先着優先）。
    #[test]
    fn fetching_concurrent_urls_keep_current() {
        let (term, _screen) = ScreenTermLike::new(100);
        let mut m =
            ProgressManager::with_draw_target(ProgressDrawTarget::term_like(Box::new(term)));

        let a = Arc::<str>::from("file:///x/aaaaaaaa");
        let b = Arc::<str>::from("file:///x/bbbbbbbb");
        m.process(Message::Cache("Fetching", a.clone()));
        m.process(Message::Cache("Fetching", b.clone()));

        // 両方とも進行中集合にいる。
        assert_eq!(m.cache_fetching.len(), 2, "both URLs are inflight");
        // current は最初の URL のまま（2番目で上書きしない）。
        assert_eq!(
            m.cache_fetching_current.as_deref(),
            Some("file:///x/aaaaaaaa"),
            "current must stay as the first URL"
        );
    }

    /// 全 URL 完了後、Fetching 行は消えず空欄になる（進行中集合は空）。
    /// 再 Fetching 時に空欄→アクティブ復帰し、正位置（fetched の前）を保つことも検証。
    #[test]
    fn fetching_bar_blanks_when_no_inflight() {
        let (term, _screen) = ScreenTermLike::new(100);
        let mut m =
            ProgressManager::with_draw_target(ProgressDrawTarget::term_like(Box::new(term)));

        let a = Arc::<str>::from("file:///x/a");
        let b = Arc::<str>::from("file:///x/b");
        m.process(Message::Cache("Fetching", a.clone()));
        m.process(Message::Cache("Fetching:done", a));

        // 進行中が尽きたので Fetching 行は空欄になる（消えず行を保つ）。
        assert!(
            m.progress_bars.contains_key(CACHE_FETCH_STAGE_ID),
            "Fetching bar must remain (blank, not cleared)"
        );
        assert!(
            m.fetch_stage_is_blank,
            "Fetching bar must be blank when no inflight"
        );
        assert!(m.cache_fetching.is_empty(), "inflight set must be empty");
        assert!(
            m.progress_bars.contains_key(CACHE_FETCH_DONE_ID),
            "fetched bar must remain"
        );

        // 別 URL を再度 Fetching → 空欄からアクティブ復帰。
        m.process(Message::Cache("Fetching", b));
        assert!(
            m.progress_bars.contains_key(CACHE_FETCH_STAGE_ID),
            "Fetching bar must still exist"
        );
        assert!(
            !m.fetch_stage_is_blank,
            "Fetching bar must resume from blank"
        );
        // 表示順で FetchStage が FetchDone より前（復帰でも順序崩れなし）。
        assert_eq!(
            ordered_children(&m),
            vec![ChildKey::FetchStage, ChildKey::FetchDone],
            "Fetching must stay above fetched"
        );
    }

    /// FetchDoneIdle は世代が一致しなければ無視される。最新の Fetching:done だけが
    /// 空欄化タイマーを発火する（タイマーは process に直接 FetchDoneIdle を送って検証）。
    #[test]
    fn fetch_done_idle_respects_generation() {
        let (term, _screen) = ScreenTermLike::new(100);
        let mut m =
            ProgressManager::with_draw_target(ProgressDrawTarget::term_like(Box::new(term)));

        let a = Arc::<str>::from("file:///x/a");
        let b = Arc::<str>::from("file:///x/b");
        m.process(Message::Cache("Fetching", a.clone()));
        m.process(Message::Cache("Fetching:done", a.clone()));
        // mark_fetch_done で世代=1、fetched 行はアクティブ表示。
        assert!(!m.fetch_done_is_blank, "fetched is active after done");

        // 現在より前の世代の idle は無視される。
        m.process(Message::FetchDoneIdle { idle_gen: 0 });
        assert!(!m.fetch_done_is_blank, "stale idle must be ignored");

        // 2件目の done で世代=2 に進む。
        m.process(Message::Cache("Fetching", b.clone()));
        m.process(Message::Cache("Fetching:done", b));
        // 1件目の世代(1)の idle は無視。
        m.process(Message::FetchDoneIdle { idle_gen: 1 });
        assert!(!m.fetch_done_is_blank, "idle with old gen must be ignored");

        // 現世代(2)の idle で空欄化。
        m.process(Message::FetchDoneIdle { idle_gen: 2 });
        assert!(
            m.fetch_done_is_blank,
            "current gen idle must blank the fetched row"
        );
    }

    /// fetched 行の罫線は Fetching の最終性に連動する:
    /// Fetching がレベル1最後(└─)なら fetched は罫線なし、
    /// Objects 等が下にあれば Fetching は中間(├─)で fetched は │。
    #[test]
    fn fetched_connector_follows_fetch_last() {
        let (term, _screen) = ScreenTermLike::new(100);
        let mut m =
            ProgressManager::with_draw_target(ProgressDrawTarget::term_like(Box::new(term)));

        let a = Arc::<str>::from("file:///x/a");

        // Objects 無し: Fetching が最後のレベル1子 → fetched は child_last（罫線なし）。
        m.process(Message::Cache("Fetching", a.clone()));
        m.process(Message::Cache("Fetching:done", a.clone()));
        let level1_last = ordered_children(&m)
            .iter()
            .rfind(|k| !matches!(k, ChildKey::FetchDone))
            .cloned();
        assert_eq!(
            level1_last,
            Some(ChildKey::FetchStage),
            "Fetching is last level1 when no Objects"
        );

        // Objects 追加: Objects が最後 → Fetching は中間 → fetched は child_mid（│）。
        m.process(Message::CacheFetchObjectsProgress {
            id: "a".into(),
            total_objs_count: 10,
            received_objs_count: 5,
        });
        let level1_last = ordered_children(&m)
            .iter()
            .rfind(|k| !matches!(k, ChildKey::FetchDone))
            .cloned();
        assert_eq!(
            level1_last,
            Some(ChildKey::FetchProgress),
            "Objects is last level1 when present"
        );
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

    #[test]
    fn loading_running_events_before_load_begin_do_not_underflow() {
        let (term, _screen) = ScreenTermLike::new(100);
        let mut m =
            ProgressManager::with_draw_target(ProgressDrawTarget::term_like(Box::new(term)));

        // EARLY は config parse のストリーミング中に始まり、LoadBegin より先に届き得る。
        m.process(Message::LoadPluginRunning);
        m.process(Message::LoadBegin { total: 1 });
        assert_eq!(
            m.loading_running.as_ref().unwrap().load(Ordering::Relaxed),
            1
        );

        m.process(Message::LoadPluginRunningDone);
        assert_eq!(
            m.loading_running.as_ref().unwrap().load(Ordering::Relaxed),
            0,
            "a pre-LoadBegin start must be paired without usize underflow"
        );
    }
}

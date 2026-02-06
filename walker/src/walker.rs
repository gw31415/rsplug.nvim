use crate::compiled_glob::{CompiledGlob, SegmentMatcher};
use hashbrown::HashSet;
use std::cmp::max;
use std::fmt;
use std::io;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::JoinSet;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug)]
pub struct WalkEvent {
    pub path: PathBuf,
    pub kind: EntryKind,
}

#[derive(Debug)]
pub enum WalkError {
    Io { path: PathBuf, source: io::Error },
    Unsupported { feature: &'static str, path: PathBuf },
}

impl fmt::Display for WalkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalkError::Io { path, source } => {
                write!(f, "io error at {}: {source}", path.display())
            }
            WalkError::Unsupported { feature, path } => {
                write!(f, "unsupported feature `{feature}` at {}", path.display())
            }
        }
    }
}

impl std::error::Error for WalkError {}

pub type WalkMessage = Result<WalkEvent, WalkError>;

#[derive(Clone, Debug)]
pub struct WalkerOptions {
    pub max_parallelism: Option<usize>,
    pub channel_capacity: usize,
}

impl Default for WalkerOptions {
    fn default() -> Self {
        Self {
            max_parallelism: None,
            channel_capacity: 1024,
        }
    }
}

struct MatchProgram {
    compiled: CompiledGlob,
}

impl MatchProgram {
    fn new(compiled: CompiledGlob) -> Self {
        Self { compiled }
    }

    fn segments(&self) -> &[SegmentMatcher] {
        self.compiled.segments()
    }
}

#[derive(Clone)]
struct TraversalCtx {
    program: Arc<MatchProgram>,
    visited: Arc<Mutex<HashSet<VisitKey>>>,
    tx: mpsc::Sender<WalkMessage>,
}

#[derive(Clone)]
struct State {
    path: PathBuf,
    seg_idx: usize,
}

#[cfg(unix)]
type DirIdentity = (u64, u64);
#[cfg(not(unix))]
type DirIdentity = PathBuf;
type VisitKey = (DirIdentity, usize);

pub struct Walker;

impl Walker {
    pub fn spawn(compiled: CompiledGlob) -> mpsc::Receiver<WalkMessage> {
        Self::spawn_with_options(compiled, WalkerOptions::default())
    }

    pub fn spawn_with_options(
        compiled: CompiledGlob,
        options: WalkerOptions,
    ) -> mpsc::Receiver<WalkMessage> {
        let (tx, rx) = mpsc::channel(options.channel_capacity.max(1));
        let max_parallelism = options.max_parallelism.unwrap_or_else(default_parallelism);
        let sem = Arc::new(Semaphore::new(max_parallelism.max(1)));
        let root = default_walk_root();
        let ctx = TraversalCtx {
            program: Arc::new(MatchProgram::new(compiled)),
            visited: Arc::new(Mutex::new(HashSet::new())),
            tx,
        };

        tokio::spawn(async move {
            let mut frontier = vec![State {
                path: root,
                seg_idx: 0,
            }];

            while !frontier.is_empty() && !ctx.tx.is_closed() {
                let current_level = std::mem::take(&mut frontier);
                let mut join_set = JoinSet::new();

                for state in current_level {
                    if ctx.tx.is_closed() {
                        break;
                    }
                    let permit = match sem.clone().acquire_owned().await {
                        Ok(permit) => permit,
                        Err(_) => break,
                    };
                    let child_ctx = ctx.clone();
                    join_set.spawn(async move {
                        process_state(child_ctx, state, permit).await
                    });
                }

                while let Some(joined) = join_set.join_next().await {
                    match joined {
                        Ok(next_states) => frontier.extend(next_states),
                        Err(err) => {
                            let _ = ctx
                                .tx
                                .send(Err(WalkError::Io {
                                    path: PathBuf::from("<join>"),
                                    source: io::Error::other(err.to_string()),
                                }))
                                .await;
                        }
                    }
                }
            }
        });

        rx
    }
}

fn default_parallelism() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|x| x.get())
        .unwrap_or(1);
    max(4, cores.saturating_mul(4))
}

fn default_walk_root() -> PathBuf {
    PathBuf::from(MAIN_SEPARATOR.to_string())
}

async fn process_state(
    ctx: TraversalCtx,
    state: State,
    _permit: OwnedSemaphorePermit,
) -> Vec<State> {
    if ctx.tx.is_closed() {
        return Vec::new();
    }

    let segments = ctx.program.segments();
    if state.seg_idx >= segments.len() {
        finalize_match(&ctx, state.path).await;
        return Vec::new();
    }

    match &segments[state.seg_idx] {
        SegmentMatcher::AnyPath(inner) => {
            let mut next_path = state.path;
            next_path.push(inner.as_str());
            vec![State {
                path: next_path,
                seg_idx: state.seg_idx + 1,
            }]
        }
        SegmentMatcher::WildMatch(matcher) => {
            let mut out = Vec::new();
            if !mark_dir_visited(&ctx.visited, &state.path, state.seg_idx).await {
                return out;
            }

            let mut dir = match tokio::fs::read_dir(&state.path).await {
                Ok(d) => d,
                Err(err) => {
                    send_error(&ctx.tx, state.path, err).await;
                    return out;
                }
            };

            while let Ok(Some(entry)) = dir.next_entry().await {
                if ctx.tx.is_closed() {
                    break;
                }
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                if matcher.matches(name) {
                    out.push(State {
                        path: entry.path(),
                        seg_idx: state.seg_idx + 1,
                    });
                }
            }
            out
        }
        SegmentMatcher::Descend => process_descend(ctx, state).await,
    }
}

#[cfg(all(unix, not(windows)))]
async fn process_descend(ctx: TraversalCtx, state: State) -> Vec<State> {
    let segments = ctx.program.segments();
    let mut next_idx = state.seg_idx + 1;
    while next_idx < segments.len() && matches!(segments[next_idx], SegmentMatcher::Descend) {
        next_idx += 1;
    }

    let root = state.path.clone();
    let (desc_tx, mut desc_rx) = mpsc::unbounded_channel();
    let walk_task = tokio::task::spawn_blocking(move || walk_descendants(&root, desc_tx));

    if next_idx >= segments.len() {
        while let Some(item) = desc_rx.recv().await {
            if ctx.tx.is_closed() {
                break;
            }
            match item {
                DescendItem::Entry(path, kind) => {
                    let _ = ctx.tx.send(Ok(WalkEvent { path, kind })).await;
                }
                DescendItem::Error(err) => {
                    send_error(&ctx.tx, state.path.clone(), err).await;
                }
            }
        }
        if let Err(err) = walk_task.await {
            send_error(&ctx.tx, state.path, io::Error::other(err.to_string())).await;
        }
        return Vec::new();
    }

    match &segments[next_idx] {
        SegmentMatcher::AnyPath(inner) => {
            let mut out = Vec::new();
            let mut root_dir_added = false;
            while let Some(item) = desc_rx.recv().await {
                let (path, kind) = match item {
                    DescendItem::Entry(path, kind) => (path, kind),
                    DescendItem::Error(err) => {
                        send_error(&ctx.tx, state.path.clone(), err).await;
                        continue;
                    }
                };
                if kind != EntryKind::Dir {
                    continue;
                }
                if !mark_known_dir_visited(&ctx.visited, &path, next_idx + 1).await {
                    continue;
                }
                out.push(State {
                    path: path.join(inner.as_str()),
                    seg_idx: next_idx + 1,
                });
                if path == state.path {
                    root_dir_added = true;
                }
            }
            if !root_dir_added && mark_dir_visited(&ctx.visited, &state.path, next_idx + 1).await
            {
                out.push(State {
                    path: state.path.join(inner.as_str()),
                    seg_idx: next_idx + 1,
                });
            }
            if let Err(err) = walk_task.await {
                send_error(&ctx.tx, state.path, io::Error::other(err.to_string())).await;
            }
            out
        }
        SegmentMatcher::WildMatch(matcher) => {
            let is_terminal = next_idx + 1 >= segments.len();
            let mut out = Vec::new();
            while let Some(item) = desc_rx.recv().await {
                let (path, kind) = match item {
                    DescendItem::Entry(path, kind) => (path, kind),
                    DescendItem::Error(err) => {
                        send_error(&ctx.tx, state.path.clone(), err).await;
                        continue;
                    }
                };
                let Some(name) = path.file_name().and_then(|x| x.to_str()) else {
                    continue;
                };
                if matcher.matches(name) {
                    if is_terminal {
                        let _ = ctx.tx.send(Ok(WalkEvent { path, kind })).await;
                    } else {
                        out.push(State {
                            path,
                            seg_idx: next_idx + 1,
                        });
                    }
                }
            }
            if let Err(err) = walk_task.await {
                send_error(&ctx.tx, state.path, io::Error::other(err.to_string())).await;
            }
            out
        }
        SegmentMatcher::Descend => Vec::new(),
    }
}

#[cfg(windows)]
async fn process_descend(_ctx: TraversalCtx, _state: State) -> Vec<State> {
    unimplemented!("Descend with fts is not implemented on Windows");
}

async fn finalize_match(ctx: &TraversalCtx, path: PathBuf) {
    match entry_kind(&path).await {
        Ok(kind) => {
            let _ = ctx.tx.send(Ok(WalkEvent { path, kind })).await;
        }
        Err(err) => {
            send_error(&ctx.tx, path, err).await;
        }
    }
}

async fn entry_kind(path: &Path) -> io::Result<EntryKind> {
    let symlink_meta = tokio::fs::symlink_metadata(path).await?;
    if symlink_meta.file_type().is_symlink() {
        return Ok(EntryKind::Symlink);
    }
    if symlink_meta.is_dir() {
        return Ok(EntryKind::Dir);
    }
    if symlink_meta.is_file() {
        return Ok(EntryKind::File);
    }
    Ok(EntryKind::Other)
}

async fn send_error(tx: &mpsc::Sender<WalkMessage>, path: PathBuf, source: io::Error) {
    let _ = tx.send(Err(WalkError::Io { path, source })).await;
}

async fn mark_dir_visited(
    visited: &Arc<Mutex<HashSet<VisitKey>>>,
    path: &Path,
    seg_idx: usize,
) -> bool {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    if !metadata.is_dir() {
        return false;
    }
    #[cfg(unix)]
    let key = {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    };
    #[cfg(not(unix))]
    let key = path.to_path_buf();
    let mut guard = visited.lock().expect("visited lock poisoned");
    guard.insert((key, seg_idx))
}

#[cfg(all(unix, not(windows)))]
async fn mark_known_dir_visited(
    visited: &Arc<Mutex<HashSet<VisitKey>>>,
    path: &Path,
    seg_idx: usize,
) -> bool {
    #[cfg(unix)]
    {
        let metadata = match tokio::fs::metadata(path).await {
            Ok(meta) => meta,
            Err(_) => return false,
        };
        use std::os::unix::fs::MetadataExt;
        let key = (metadata.dev(), metadata.ino());
        let mut guard = visited.lock().expect("visited lock poisoned");
        guard.insert((key, seg_idx))
    }
    #[cfg(not(unix))]
    {
        let mut guard = visited.lock().expect("visited lock poisoned");
        guard.insert((path.to_path_buf(), seg_idx))
    }
}

#[cfg(all(unix, not(windows)))]
enum DescendItem {
    Entry(PathBuf, EntryKind),
    Error(io::Error),
}

#[cfg(all(unix, not(windows)))]
fn walk_descendants(root: &Path, tx: mpsc::UnboundedSender<DescendItem>) {
    use fts::walkdir::{WalkDir, WalkDirConf};
    use std::io::ErrorKind;

    let iter = WalkDir::new(WalkDirConf::new(root).follow_symlink().no_chdir()).into_iter();
    for item in iter {
        match item {
            Ok(entry) => {
                let path = entry.path().to_path_buf();
                if path == root {
                    let _ = tx.send(DescendItem::Entry(path, EntryKind::Dir));
                    continue;
                }
                let ftype = entry.file_type();
                let kind = if ftype.is_symlink() {
                    EntryKind::Symlink
                } else if ftype.is_dir() {
                    EntryKind::Dir
                } else if ftype.is_file() {
                    EntryKind::File
                } else {
                    EntryKind::Other
                };
                let _ = tx.send(DescendItem::Entry(path, kind));
            }
            Err(err) => {
                let kind = err.kind();
                if matches!(kind, ErrorKind::PermissionDenied | ErrorKind::NotFound) {
                    continue;
                }
                let _ = tx.send(DescendItem::Error(err));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(all(unix, not(windows)))]
    use super::*;
    use crate::compiled_glob::CompiledGlob;
    #[cfg(all(unix, not(windows)))]
    use std::path::PathBuf;
    #[cfg(all(unix, not(windows)))]
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(all(unix, not(windows)))]
    use std::collections::BTreeSet;
    #[cfg(all(unix, not(windows)))]
    use std::fs;
    #[cfg(all(unix, not(windows)))]
    use std::time::Duration;

    #[cfg(all(unix, not(windows)))]
    fn test_root(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        std::env::temp_dir().join(format!("walker-{name}-{stamp}"))
    }

    #[tokio::test]
    async fn descend_match_equivalence() {
        let one = CompiledGlob::new("a/**/**/b").expect("glob must parse");
        let two = CompiledGlob::new("a/**/b").expect("glob must parse");
        for path in [
            "a/b",
            "a/x/b",
            "a/x/y/b",
            "a/x/y/c",
            "x/a/b",
            "a/x/y/z",
        ] {
            assert_eq!(one.r#match(path.as_ref()), two.r#match(path.as_ref()));
        }
    }

    #[tokio::test]
    #[cfg(all(unix, not(windows)))]
    async fn streams_results_before_full_walk() {
        let root = test_root("stream");
        fs::create_dir_all(root.join("d1/d2")).expect("create tree");
        fs::write(root.join("d1/a.txt"), b"a").expect("write file");
        fs::write(root.join("d1/d2/b.txt"), b"b").expect("write file");

        let pattern = format!("{}/**", root.display());
        let glob = CompiledGlob::new(&pattern).expect("glob must parse");
        let mut rx = Walker::spawn(glob);

        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("must receive quickly");
        assert!(first.is_some());

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[cfg(all(unix, not(windows)))]
    async fn finds_expected_paths() {
        let root = test_root("paths");
        fs::create_dir_all(root.join("src/bin")).expect("create tree");
        fs::create_dir_all(root.join("docs")).expect("create tree");
        fs::write(root.join("src/main.rs"), b"fn main(){}").expect("write file");
        fs::write(root.join("src/bin/tool.rs"), b"fn main(){}").expect("write file");
        fs::write(root.join("docs/readme.md"), b"# hi").expect("write file");

        let pattern = format!("{}/**/*.rs", root.display());
        let glob = CompiledGlob::new(&pattern).expect("glob must parse");
        let mut rx = Walker::spawn(glob);

        let mut got = BTreeSet::new();
        while let Some(msg) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("channel should respond")
        {
            if let Ok(ev) = msg {
                got.insert(
                    ev.path
                        .strip_prefix(&root)
                        .expect("path under root")
                        .to_path_buf(),
                );
            }
        }

        let expected: BTreeSet<PathBuf> = ["src/main.rs", "src/bin/tool.rs"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(got, expected);
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[cfg(all(unix, not(windows)))]
    async fn symlink_loop_does_not_hang() {
        use std::os::unix::fs::symlink;

        let root = test_root("loop");
        fs::create_dir_all(root.join("a/b")).expect("create tree");
        fs::write(root.join("a/b/file.txt"), b"1").expect("write file");
        symlink(root.join("a"), root.join("a/b/link_to_a")).expect("create symlink");

        let pattern = format!("{}/**", root.display());
        let glob = CompiledGlob::new(&pattern).expect("glob must parse");
        let mut rx = Walker::spawn(glob);

        let mut count = 0usize;
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
            count += 1;
            if count > 64 {
                break;
            }
        }
        assert!(count > 0);
        assert!(count <= 64);

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[cfg(all(unix, not(windows)))]
    async fn permission_denied_does_not_abort_descend_walk() {
        use std::os::unix::fs::PermissionsExt;

        let root = test_root("perm");
        fs::create_dir_all(root.join("ok")).expect("create tree");
        fs::create_dir_all(root.join("blocked")).expect("create tree");
        fs::write(root.join("ok/keep.rs"), b"fn main(){}").expect("write file");
        fs::set_permissions(root.join("blocked"), fs::Permissions::from_mode(0o0))
            .expect("chmod blocked");

        let pattern = format!("{}/**.rs", root.display());
        let glob = CompiledGlob::new(&pattern).expect("glob must parse");
        let mut rx = Walker::spawn(glob);

        let mut got_ok = false;
        while let Some(msg) = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("channel should respond")
        {
            match msg {
                Ok(ev) => {
                    if ev.path == root.join("ok/keep.rs") {
                        got_ok = true;
                    }
                }
                Err(WalkError::Io { .. }) => {}
                Err(WalkError::Unsupported { .. }) => {}
            }
        }

        assert!(got_ok, "accessible matches should still be emitted");

        fs::set_permissions(root.join("blocked"), fs::Permissions::from_mode(0o755))
            .expect("restore perms");
        let _ = fs::remove_dir_all(&root);
    }
}

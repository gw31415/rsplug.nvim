use crate::compiled_glob::CompiledGlob;
use hashbrown::HashSet;
use std::cmp::max;
use std::fmt;
use std::hash::{Hash, Hasher};
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
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Unsupported {
        feature: &'static str,
        path: PathBuf,
    },
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
    pub files_only: bool,
}

impl Default for WalkerOptions {
    fn default() -> Self {
        Self {
            max_parallelism: None,
            channel_capacity: 1024,
            files_only: false,
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

    fn initial_states(&self) -> Vec<usize> {
        self.compiled.initial_states()
    }

    fn advance_states(&self, current: &[usize], part: &str) -> Vec<usize> {
        self.compiled.advance_states(current, part)
    }

    fn is_match_state(&self, current: &[usize]) -> bool {
        self.compiled.is_match_state(current)
    }

    fn literal_candidates(&self, current: &[usize]) -> Vec<String> {
        self.compiled.literal_candidates(current)
    }

    fn needs_directory_scan(&self, current: &[usize]) -> bool {
        self.compiled.needs_directory_scan(current)
    }
}

#[derive(Clone)]
struct TraversalCtx {
    program: Arc<MatchProgram>,
    visited: Arc<Mutex<HashSet<VisitKey>>>,
    tx: mpsc::Sender<WalkMessage>,
    files_only: bool,
}

#[derive(Clone)]
struct State {
    path: PathBuf,
    match_states: Vec<usize>,
}

#[cfg(unix)]
type DirIdentity = (u64, u64);
#[cfg(not(unix))]
type DirIdentity = PathBuf;
type VisitKey = (DirIdentity, u64);

pub struct Walker;

impl Walker {
    pub fn spawn(compiled: CompiledGlob) -> mpsc::Receiver<WalkMessage> {
        Self::spawn_with_options(compiled, WalkerOptions::default())
    }

    pub fn spawn_many(
        globs: impl IntoIterator<Item = CompiledGlob>,
    ) -> mpsc::Receiver<WalkMessage> {
        Self::spawn_many_with_options(globs, WalkerOptions::default())
    }

    pub fn spawn_with_options(
        compiled: CompiledGlob,
        options: WalkerOptions,
    ) -> mpsc::Receiver<WalkMessage> {
        Self::spawn_many_with_options([compiled], options)
    }

    pub fn spawn_many_with_options(
        globs: impl IntoIterator<Item = CompiledGlob>,
        options: WalkerOptions,
    ) -> mpsc::Receiver<WalkMessage> {
        let merged = match CompiledGlob::merge_many(globs) {
            Ok(merged) => merged,
            Err(err) => {
                let (tx, rx) = mpsc::channel(options.channel_capacity.max(1));
                tokio::spawn(async move {
                    let _ = tx
                        .send(Err(WalkError::Io {
                            path: PathBuf::from("<spawn_many>"),
                            source: err,
                        }))
                        .await;
                });
                return rx;
            }
        };

        Self::spawn_single_with_options(merged, options)
    }

    fn spawn_single_with_options(
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
            files_only: options.files_only,
        };
        let initial_states = ctx.program.initial_states();

        tokio::spawn(async move {
            let mut frontier = vec![State {
                path: root,
                match_states: initial_states,
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
                    join_set.spawn(async move { process_state(child_ctx, state, permit).await });
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

fn states_signature(states: &[usize]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for state in states {
        state.hash(&mut hasher);
    }
    hasher.finish()
}

async fn process_state(
    ctx: TraversalCtx,
    state: State,
    _permit: OwnedSemaphorePermit,
) -> Vec<State> {
    if ctx.tx.is_closed() || state.match_states.is_empty() {
        return Vec::new();
    }

    if ctx.program.is_match_state(&state.match_states) {
        finalize_match(&ctx, state.path.clone()).await;
    }

    let signature = states_signature(&state.match_states);
    let mut out = Vec::new();
    let literal_candidates = ctx.program.literal_candidates(&state.match_states);
    let mut handled_names = HashSet::new();
    for literal in literal_candidates {
        handled_names.insert(literal.clone());
        let next_states = ctx.program.advance_states(&state.match_states, &literal);
        if next_states.is_empty() {
            continue;
        }
        let candidate_path = state.path.join(&literal);
        match tokio::fs::symlink_metadata(&candidate_path).await {
            Ok(_) => out.push(State {
                path: candidate_path,
                match_states: next_states,
            }),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::PermissionDenied
                        | io::ErrorKind::NotADirectory
                ) => {}
            Err(err) => {
                send_error(&ctx.tx, candidate_path, err).await;
            }
        }
    }

    if !ctx.program.needs_directory_scan(&state.match_states) {
        return out;
    }

    if !mark_dir_visited(&ctx.visited, &state.path, signature).await {
        return out;
    }

    let mut dir = match tokio::fs::read_dir(&state.path).await {
        Ok(d) => d,
        Err(err) if err.kind() == io::ErrorKind::NotADirectory => {
            return out;
        }
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
        if handled_names.contains(name) {
            continue;
        }
        let next_states = ctx.program.advance_states(&state.match_states, name);
        if next_states.is_empty() {
            continue;
        }
        out.push(State {
            path: entry.path(),
            match_states: next_states,
        });
    }

    out
}

async fn finalize_match(ctx: &TraversalCtx, path: PathBuf) {
    match entry_kind(&path).await {
        Ok(kind) => {
            if ctx.files_only && kind != EntryKind::File {
                return;
            }
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
    signature: u64,
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
    guard.insert((key, signature))
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
        for path in ["a/b", "a/x/b", "a/x/y/b", "a/x/y/c", "x/a/b", "a/x/y/z"] {
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

    #[tokio::test]
    #[cfg(all(unix, not(windows)))]
    async fn spawn_many_matches_union_of_patterns() {
        let root = test_root("many_union");
        fs::create_dir_all(root.join("src")).expect("create tree");
        fs::create_dir_all(root.join("docs")).expect("create tree");
        fs::write(root.join("src/main.rs"), b"fn main(){}").expect("write file");
        fs::write(root.join("docs/readme.md"), b"# hi").expect("write file");

        let g1 =
            CompiledGlob::new(&format!("{}/**/*.rs", root.display())).expect("glob must parse");
        let g2 =
            CompiledGlob::new(&format!("{}/**/*.md", root.display())).expect("glob must parse");
        let mut rx = Walker::spawn_many(vec![g1, g2]);

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

        let expected: BTreeSet<PathBuf> = ["src/main.rs", "docs/readme.md"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(got, expected);
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[cfg(all(unix, not(windows)))]
    async fn spawn_many_single_equivalent_to_spawn() {
        let root = test_root("many_single");
        fs::create_dir_all(root.join("src/bin")).expect("create tree");
        fs::write(root.join("src/main.rs"), b"fn main(){}").expect("write file");
        fs::write(root.join("src/bin/tool.rs"), b"fn main(){}").expect("write file");

        let pattern = format!("{}/**/*.rs", root.display());
        let glob = CompiledGlob::new(&pattern).expect("glob must parse");

        let mut single_rx = Walker::spawn(CompiledGlob::new(&pattern).expect("glob must parse"));
        let mut many_rx = Walker::spawn_many(vec![glob]);

        let mut single = BTreeSet::new();
        while let Some(msg) = tokio::time::timeout(Duration::from_secs(2), single_rx.recv())
            .await
            .expect("channel should respond")
        {
            if let Ok(ev) = msg {
                single.insert(
                    ev.path
                        .strip_prefix(&root)
                        .expect("path under root")
                        .to_path_buf(),
                );
            }
        }

        let mut many = BTreeSet::new();
        while let Some(msg) = tokio::time::timeout(Duration::from_secs(2), many_rx.recv())
            .await
            .expect("channel should respond")
        {
            if let Ok(ev) = msg {
                many.insert(
                    ev.path
                        .strip_prefix(&root)
                        .expect("path under root")
                        .to_path_buf(),
                );
            }
        }

        assert_eq!(single, many);
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[cfg(all(unix, not(windows)))]
    async fn spawn_many_duplicate_patterns_no_duplicate_path() {
        let root = test_root("many_dup");
        fs::create_dir_all(root.join("src")).expect("create tree");
        fs::write(root.join("src/main.rs"), b"fn main(){}").expect("write file");

        let pattern = format!("{}/**/*.rs", root.display());
        let g1 = CompiledGlob::new(&pattern).expect("glob must parse");
        let g2 = CompiledGlob::new(&pattern).expect("glob must parse");
        let mut rx = Walker::spawn_many(vec![g1, g2]);

        let mut count = 0usize;
        while let Some(msg) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("channel should respond")
        {
            if let Ok(ev) = msg
                && ev.path.ends_with("src/main.rs")
            {
                count += 1;
            }
        }

        assert_eq!(count, 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[cfg(all(unix, not(windows)))]
    async fn spawn_many_applies_last_match_wins_with_excludes() {
        let root = test_root("many_exclude");
        fs::create_dir_all(root.join("target")).expect("create tree");
        fs::write(root.join("target/keep.txt"), b"x").expect("write file");
        fs::write(root.join("target/ignore.txt"), b"x").expect("write file");

        let include =
            CompiledGlob::new(&format!("{}/**/*.txt", root.display())).expect("glob must parse");
        let exclude = CompiledGlob::new(&format!("!{}/**/ignore.txt", root.display()))
            .expect("glob must parse");
        let reinclude = CompiledGlob::new(&format!("{}/**/ignore.txt", root.display()))
            .expect("glob must parse");
        let mut rx = Walker::spawn_many(vec![include, exclude, reinclude]);

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

        let expected: BTreeSet<PathBuf> = ["target/keep.txt", "target/ignore.txt"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(got, expected);
        let _ = fs::remove_dir_all(&root);
    }
}

use crate::compiled_glob::CompiledGlob;
use crate::walker::{EntryKind, WalkError, WalkEvent, WalkMessage, WalkerOptions};
use fts::fts::{Fts, FtsInfo, FtsSetOption, fts_option};
use hashbrown::HashMap;
use std::collections::{HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use tokio::sync::mpsc;
#[cfg(not(feature = "bench-persistent-workers"))]
use tokio::task::JoinSet;

const TRANSITION_CACHE_CAPACITY: usize = 64 * 1024;
const STATE_CACHE_CAPACITY: usize = 64 * 1024;
const EMIT_BATCH_SIZE: usize = 128;
const SHARD_FACTOR: usize = 6;
const SHARD_DEPTH: usize = 2;
const SPLIT_DEPTH_LIMIT: usize = 2;
const SPLIT_BACKLOG_FACTOR: usize = 4;
const SPLIT_MIN_CHILDREN: usize = 24;
const QUEUE_WAIT_MILLIS: u64 = 5;

#[derive(Clone)]
struct RootJob {
    path: PathBuf,
    root_states: Vec<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
struct TransitionKey {
    state_sig: u64,
    name_sig: u64,
    name_len: u16,
}

struct TransitionValue {
    name: Vec<u8>,
    parent_states: Arc<[usize]>,
    states: Arc<[usize]>,
    next_sig: u64,
}

#[derive(Default)]
struct StateEvalCache {
    match_cache: HashMap<u64, bool>,
    scan_cache: HashMap<u64, bool>,
}

enum WorkerMessage {
    Events(Vec<WalkEvent>),
    Error(WalkError),
}

#[derive(Default)]
struct JobQueueInner {
    queue: VecDeque<RootJob>,
    closed: bool,
}

struct JobQueue {
    inner: Mutex<JobQueueInner>,
    cv: Condvar,
}

impl JobQueue {
    fn new(init: Vec<RootJob>) -> Self {
        Self {
            inner: Mutex::new(JobQueueInner {
                queue: init.into(),
                closed: false,
            }),
            cv: Condvar::new(),
        }
    }

    fn push(&self, job: RootJob) -> bool {
        let mut inner = self.inner.lock().expect("job queue lock");
        if inner.closed {
            return false;
        }
        inner.queue.push_back(job);
        self.cv.notify_one();
        true
    }

    fn pop(&self, cancel: &AtomicBool, active_jobs: &AtomicUsize) -> Option<RootJob> {
        let mut inner = self.inner.lock().expect("job queue lock");
        loop {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            if let Some(job) = inner.queue.pop_front() {
                return Some(job);
            }
            if inner.closed || active_jobs.load(Ordering::Relaxed) == 0 {
                return None;
            }
            let (guard, _) = self
                .cv
                .wait_timeout(inner, std::time::Duration::from_millis(QUEUE_WAIT_MILLIS))
                .expect("job queue wait");
            inner = guard;
        }
    }

    fn close(&self) {
        let mut inner = self.inner.lock().expect("job queue lock");
        inner.closed = true;
        self.cv.notify_all();
    }
}

struct WorkerCtx {
    compiled: Arc<CompiledGlob>,
    files_only: bool,
    cancel: Arc<AtomicBool>,
    active_jobs: Arc<AtomicUsize>,
    queue: Arc<JobQueue>,
    worker_tx: mpsc::Sender<WorkerMessage>,
    split_backlog_limit: usize,
}

#[cfg(feature = "bench-persistent-workers")]
mod persistent_pool {
    use std::sync::{Arc, Mutex, OnceLock, mpsc};
    use tokio::sync::oneshot;

    type Job = Box<dyn FnOnce() + Send + 'static>;

    pub(super) struct PersistentPool {
        tx: mpsc::Sender<Job>,
    }

    impl PersistentPool {
        fn new(thread_count: usize) -> Self {
            let (tx, rx) = mpsc::channel::<Job>();
            let rx = Arc::new(Mutex::new(rx));

            for idx in 0..thread_count.max(1) {
                let rx = Arc::clone(&rx);
                std::thread::Builder::new()
                    .name(format!("walker-bench-{idx}"))
                    .spawn(move || {
                        loop {
                            let job = {
                                let guard = rx.lock().expect("persistent queue lock");
                                guard.recv()
                            };
                            match job {
                                Ok(job) => job(),
                                Err(_) => break,
                            }
                        }
                    })
                    .expect("failed to spawn persistent bench worker");
            }

            Self { tx }
        }

        pub(super) fn spawn<F>(&self, f: F) -> oneshot::Receiver<bool>
        where
            F: FnOnce() + Send + 'static,
        {
            let (tx, rx) = oneshot::channel();
            let _ = self.tx.send(Box::new(move || {
                let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_ok();
                let _ = tx.send(ok);
            }));
            rx
        }
    }

    pub(super) fn global(thread_count: usize) -> &'static PersistentPool {
        static POOL: OnceLock<PersistentPool> = OnceLock::new();
        POOL.get_or_init(|| PersistentPool::new(thread_count))
    }
}

pub(super) fn spawn_single_with_options(
    compiled: CompiledGlob,
    options: WalkerOptions,
) -> mpsc::Receiver<WalkMessage> {
    let (tx, rx) = mpsc::channel(options.channel_capacity.max(1));

    tokio::spawn(async move {
        let compiled = Arc::new(compiled);
        let files_only = options.files_only;
        let max_parallelism = options
            .max_parallelism
            .unwrap_or_else(default_parallelism)
            .max(1);
        let max_jobs = max_parallelism.saturating_mul(SHARD_FACTOR).max(1);

        let prepared = tokio::task::spawn_blocking({
            let compiled = Arc::clone(&compiled);
            move || prepare_jobs(compiled.as_ref(), files_only, max_jobs)
        })
        .await;

        let (jobs, initial_events) = match prepared {
            Ok(value) => value,
            Err(err) => {
                let _ = tx
                    .send(Err(WalkError::Io {
                        path: PathBuf::from("<prepare_jobs>"),
                        source: io::Error::other(err.to_string()),
                    }))
                    .await;
                return;
            }
        };

        let cancel = Arc::new(AtomicBool::new(false));
        for event in initial_events {
            if tx.send(Ok(event)).await.is_err() {
                cancel.store(true, Ordering::Relaxed);
                return;
            }
        }

        if jobs.is_empty() {
            return;
        }

        let active_jobs = Arc::new(AtomicUsize::new(jobs.len()));
        let queue = Arc::new(JobQueue::new(jobs));
        let split_backlog_limit = max_parallelism.saturating_mul(SPLIT_BACKLOG_FACTOR).max(1);

        let (worker_tx, worker_rx) =
            mpsc::channel::<WorkerMessage>(options.channel_capacity.max(1));
        let forward_cancel = Arc::clone(&cancel);
        let tx_forward = tx.clone();
        let forwarder = tokio::spawn(async move {
            forward_worker_messages(worker_rx, tx_forward, forward_cancel).await;
        });

        #[cfg(not(feature = "bench-persistent-workers"))]
        let mut worker_set = JoinSet::new();
        #[cfg(feature = "bench-persistent-workers")]
        let mut waiters = Vec::with_capacity(max_parallelism);

        for _ in 0..max_parallelism {
            let ctx = WorkerCtx {
                compiled: Arc::clone(&compiled),
                files_only,
                cancel: Arc::clone(&cancel),
                active_jobs: Arc::clone(&active_jobs),
                queue: Arc::clone(&queue),
                worker_tx: worker_tx.clone(),
                split_backlog_limit,
            };
            #[cfg(not(feature = "bench-persistent-workers"))]
            worker_set.spawn_blocking(move || run_worker(ctx));
            #[cfg(feature = "bench-persistent-workers")]
            waiters.push(persistent_pool::global(max_parallelism).spawn(move || run_worker(ctx)));
        }
        drop(worker_tx);

        #[cfg(not(feature = "bench-persistent-workers"))]
        while let Some(joined) = worker_set.join_next().await {
            if let Err(err) = joined {
                cancel.store(true, Ordering::Relaxed);
                queue.close();
                let _ = tx
                    .send(Err(WalkError::Io {
                        path: PathBuf::from("<join_worker>"),
                        source: io::Error::other(err.to_string()),
                    }))
                    .await;
            }
        }

        #[cfg(feature = "bench-persistent-workers")]
        for waiter in waiters {
            match waiter.await {
                Ok(true) => {}
                Ok(false) | Err(_) => {
                    cancel.store(true, Ordering::Relaxed);
                    queue.close();
                    let _ = tx
                        .send(Err(WalkError::Io {
                            path: PathBuf::from("<join_worker>"),
                            source: io::Error::other("persistent worker panicked"),
                        }))
                        .await;
                }
            }
        }

        // Wait for the forwarder to drain all pending events before finishing.
        let _ = forwarder.await;
    });

    rx
}

fn run_worker(ctx: WorkerCtx) {
    loop {
        if ctx.cancel.load(Ordering::Relaxed) {
            return;
        }

        let Some(job) = ctx.queue.pop(&ctx.cancel, &ctx.active_jobs) else {
            return;
        };

        run_fts_job(&ctx, job);

        if ctx.active_jobs.fetch_sub(1, Ordering::AcqRel) == 1 {
            ctx.queue.close();
            return;
        }
    }
}

fn run_fts_job(ctx: &WorkerCtx, job: RootJob) {
    if ctx.cancel.load(Ordering::Relaxed) {
        return;
    }

    let root_string = job.path.to_string_lossy().to_string();
    let mut fts = match Fts::new(
        vec![root_string],
        fts_option::Flags::PHYSICAL | fts_option::Flags::NOCHDIR,
        None,
    ) {
        Ok(fts) => fts,
        Err(err) => {
            let _ = ctx
                .worker_tx
                .blocking_send(WorkerMessage::Error(WalkError::Io {
                    path: job.path,
                    source: io::Error::other(format!("failed to initialize fts: {err:?}")),
                }));
            return;
        }
    };

    let mut level_states: Vec<Arc<[usize]>> = Vec::new();
    let mut transition_cache: HashMap<TransitionKey, TransitionValue> = HashMap::new();
    let mut transition_cache_len = 0usize;
    let mut state_cache = StateEvalCache::default();
    let mut pending_events = Vec::with_capacity(EMIT_BATCH_SIZE);
    let mut next_states_scratch = Vec::new();

    while let Some(entry) = fts.read() {
        if ctx.cancel.load(Ordering::Relaxed) {
            return;
        }

        let level = match usize::try_from(entry.level) {
            Ok(level) => level,
            Err(_) => continue,
        };

        match entry.info {
            FtsInfo::IsDot | FtsInfo::IsDirPost => {
                flush_events(&ctx.worker_tx, &mut pending_events, &ctx.cancel);
                if level < level_states.len() {
                    level_states.truncate(level);
                }
                continue;
            }
            FtsInfo::IsErr | FtsInfo::IsDontRead | FtsInfo::IsNoStat => {
                flush_events(&ctx.worker_tx, &mut pending_events, &ctx.cancel);
                let source = if entry.error == 0 {
                    io::Error::other("fts reported an unreadable entry")
                } else {
                    io::Error::from_raw_os_error(entry.error)
                };
                let _ = ctx
                    .worker_tx
                    .blocking_send(WorkerMessage::Error(WalkError::Io {
                        path: entry.path.clone(),
                        source,
                    }));
                continue;
            }
            _ => {}
        }

        let is_dir = matches!(entry.info, FtsInfo::IsDir | FtsInfo::IsDirCyclic);
        let (states, states_sig) = if level == 0 {
            let states = Arc::<[usize]>::from(job.root_states.clone());
            let signature = states_signature(states.as_ref());
            (states, signature)
        } else {
            let parent = match level_states.get(level.saturating_sub(1)) {
                Some(parent) => parent,
                None => {
                    if is_dir {
                        let _ = fts.set(&entry, FtsSetOption::Skip);
                    }
                    continue;
                }
            };
            if parent.is_empty() {
                if is_dir {
                    let _ = fts.set(&entry, FtsSetOption::Skip);
                }
                continue;
            }

            let name_bytes = entry.name.as_os_str().as_bytes();
            let name_len = match u16::try_from(name_bytes.len()) {
                Ok(v) => v,
                Err(_) => {
                    if is_dir {
                        let _ = fts.set(&entry, FtsSetOption::Skip);
                    }
                    continue;
                }
            };

            let key = TransitionKey {
                state_sig: states_signature(parent.as_ref()),
                name_sig: bytes_signature(name_bytes),
                name_len,
            };

            if let Some(cached) = transition_cache.get(&key)
                && cached.name.as_slice() == name_bytes
                && cached.parent_states.as_ref() == parent.as_ref()
            {
                (Arc::clone(&cached.states), cached.next_sig)
            } else {
                let Some(name) = entry.name.to_str() else {
                    if is_dir {
                        let _ = fts.set(&entry, FtsSetOption::Skip);
                    }
                    continue;
                };

                ctx.compiled
                    .advance_states_into(parent.as_ref(), name, &mut next_states_scratch);
                let next_sig = states_signature(&next_states_scratch);
                let next_states = Arc::<[usize]>::from(next_states_scratch.clone());

                if transition_cache_len >= TRANSITION_CACHE_CAPACITY {
                    transition_cache.clear();
                    transition_cache_len = 0;
                }
                if transition_cache
                    .insert(
                        key,
                        TransitionValue {
                            name: name_bytes.to_vec(),
                            parent_states: Arc::clone(parent),
                            states: Arc::clone(&next_states),
                            next_sig,
                        },
                    )
                    .is_none()
                {
                    transition_cache_len += 1;
                }

                (next_states, next_sig)
            }
        };

        if level_states.len() <= level {
            level_states.resize(level + 1, Arc::<[usize]>::from(Vec::<usize>::new()));
        }
        level_states[level] = Arc::clone(&states);
        level_states.truncate(level + 1);
        let states = level_states[level].as_ref();

        if states.is_empty() {
            if is_dir {
                let _ = fts.set(&entry, FtsSetOption::Skip);
            }
            continue;
        }

        if is_dir
            && !cached_needs_directory_scan(
                &mut state_cache,
                ctx.compiled.as_ref(),
                states_sig,
                states,
            )
        {
            let _ = fts.set(&entry, FtsSetOption::Skip);
            continue;
        }

        let is_match =
            cached_is_match_state(&mut state_cache, ctx.compiled.as_ref(), states_sig, states);

        if is_dir
            && level > 0
            && should_split_directory(
                entry.path.as_path(),
                level,
                &ctx.active_jobs,
                ctx.split_backlog_limit,
            )
        {
            if is_match && !ctx.files_only {
                pending_events.push(WalkEvent {
                    path: entry.path.clone(),
                    kind: EntryKind::Dir,
                });
            }

            ctx.active_jobs.fetch_add(1, Ordering::AcqRel);
            let enqueued = ctx.queue.push(RootJob {
                path: entry.path.clone(),
                root_states: states.to_vec(),
            });
            if !enqueued {
                ctx.active_jobs.fetch_sub(1, Ordering::AcqRel);
            }
            let _ = fts.set(&entry, FtsSetOption::Skip);

            if pending_events.len() >= EMIT_BATCH_SIZE {
                flush_events(&ctx.worker_tx, &mut pending_events, &ctx.cancel);
            }
            continue;
        }

        if ctx.files_only && is_dir {
            continue;
        }

        if is_match {
            let kind = entry_kind(entry.info.clone());
            pending_events.push(WalkEvent {
                path: entry.path.clone(),
                kind,
            });
            if pending_events.len() >= EMIT_BATCH_SIZE {
                flush_events(&ctx.worker_tx, &mut pending_events, &ctx.cancel);
            }
        }
    }

    flush_events(&ctx.worker_tx, &mut pending_events, &ctx.cancel);
}

async fn forward_worker_messages(
    mut rx: mpsc::Receiver<WorkerMessage>,
    tx: mpsc::Sender<WalkMessage>,
    cancel: Arc<AtomicBool>,
) {
    while let Some(msg) = rx.recv().await {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        match msg {
            WorkerMessage::Events(events) => {
                for event in events {
                    if tx.send(Ok(event)).await.is_err() {
                        cancel.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            }
            WorkerMessage::Error(err) => {
                if tx.send(Err(err)).await.is_err() {
                    cancel.store(true, Ordering::Relaxed);
                    return;
                }
            }
        }
    }
}

fn should_split_directory(
    path: &Path,
    depth: usize,
    active_jobs: &AtomicUsize,
    split_backlog_limit: usize,
) -> bool {
    if depth > SPLIT_DEPTH_LIMIT {
        return false;
    }
    if active_jobs.load(Ordering::Relaxed) >= split_backlog_limit {
        return false;
    }
    has_min_children(path, SPLIT_MIN_CHILDREN)
}

fn has_min_children(path: &Path, min_children: usize) -> bool {
    let mut count = 0usize;
    let Ok(read_dir) = std::fs::read_dir(path) else {
        return false;
    };
    for entry in read_dir {
        if entry.is_err() {
            continue;
        }
        count += 1;
        if count >= min_children {
            return true;
        }
    }
    false
}

fn flush_events(
    tx: &mpsc::Sender<WorkerMessage>,
    pending: &mut Vec<WalkEvent>,
    cancel: &AtomicBool,
) {
    if pending.is_empty() || cancel.load(Ordering::Relaxed) {
        pending.clear();
        return;
    }
    let chunk = std::mem::take(pending);
    if tx.blocking_send(WorkerMessage::Events(chunk)).is_err() {
        cancel.store(true, Ordering::Relaxed);
    }
}

fn prepare_jobs(
    compiled: &CompiledGlob,
    files_only: bool,
    max_jobs: usize,
) -> (Vec<RootJob>, Vec<WalkEvent>) {
    let roots = normalize_roots(compiled.start_paths());
    let mut jobs = Vec::new();
    let mut initial_events = Vec::new();
    let mut state_cache = StateEvalCache::default();

    for root in roots {
        if jobs.len() >= max_jobs {
            break;
        }

        let metadata = match std::fs::metadata(root.as_path()) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => continue,
        };
        if !metadata.is_dir() {
            continue;
        }

        let root_states = compiled.states_for_path(root.as_path());
        if root_states.is_empty() {
            continue;
        }

        let sharded = shard_root_jobs(
            compiled,
            root.as_path(),
            &root_states,
            files_only,
            max_jobs,
            SHARD_DEPTH,
            &mut state_cache,
            &mut jobs,
            &mut initial_events,
        );

        if !sharded {
            jobs.push(RootJob {
                path: root,
                root_states,
            });
        } else if cached_is_match_state(
            &mut state_cache,
            compiled,
            states_signature(&root_states),
            &root_states,
        ) && !files_only
        {
            initial_events.push(WalkEvent {
                path: root,
                kind: EntryKind::Dir,
            });
        }
    }

    (jobs, initial_events)
}

fn shard_root_jobs(
    compiled: &CompiledGlob,
    root: &Path,
    root_states: &[usize],
    files_only: bool,
    max_jobs: usize,
    depth: usize,
    state_cache: &mut StateEvalCache,
    jobs: &mut Vec<RootJob>,
    initial_events: &mut Vec<WalkEvent>,
) -> bool {
    if depth == 0 || jobs.len() >= max_jobs {
        return false;
    }

    let mut reader = match std::fs::read_dir(root) {
        Ok(reader) => reader,
        Err(_) => return false,
    };

    let mut local_jobs = Vec::new();
    let mut local_events = Vec::new();
    let mut split_happened = false;

    let mut capacity_exhausted = false;

    while let Some(entry) = reader.next().transpose().ok().flatten() {
        if jobs.len() + local_jobs.len() >= max_jobs {
            capacity_exhausted = true;
            break;
        }

        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return false;
        };

        let next_states = compiled.advance_states(root_states, name);
        if next_states.is_empty() {
            continue;
        }

        let kind = classify_entry(entry.path().as_path(), entry.file_type().ok());
        let next_signature = states_signature(&next_states);
        if kind != Some(EntryKind::Dir)
            || !cached_needs_directory_scan(state_cache, compiled, next_signature, &next_states)
        {
            if cached_is_match_state(state_cache, compiled, next_signature, &next_states)
                && let Some(kind) = kind
                && (!files_only || kind == EntryKind::File)
            {
                local_events.push(WalkEvent {
                    path: entry.path(),
                    kind,
                });
            }
            continue;
        }

        if depth > 1 {
            let child_before = local_jobs.len();
            let mut child_jobs = Vec::new();
            let mut child_events = Vec::new();
            let child_split = shard_root_jobs(
                compiled,
                entry.path().as_path(),
                &next_states,
                files_only,
                max_jobs.saturating_sub(jobs.len()),
                depth - 1,
                state_cache,
                &mut child_jobs,
                &mut child_events,
            );
            if child_split {
                local_jobs.extend(child_jobs);
                local_events.extend(child_events);
                split_happened = true;
                continue;
            }
            if local_jobs.len() > child_before {
                local_jobs.truncate(child_before);
            }
        }

        local_jobs.push(RootJob {
            path: entry.path(),
            root_states: next_states,
        });
        split_happened = true;
    }

    if capacity_exhausted {
        return false;
    }

    if split_happened {
        jobs.extend(
            local_jobs
                .into_iter()
                .take(max_jobs.saturating_sub(jobs.len())),
        );
        initial_events.extend(local_events);
    }

    split_happened
}

fn classify_entry(path: &Path, file_type: Option<std::fs::FileType>) -> Option<EntryKind> {
    let file_type = file_type?;
    if file_type.is_dir() {
        return Some(EntryKind::Dir);
    }
    if file_type.is_file() {
        return Some(EntryKind::File);
    }
    if file_type.is_symlink() {
        return symlink_kind(path);
    }
    Some(EntryKind::Other)
}

fn symlink_kind(path: &Path) -> Option<EntryKind> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Some(EntryKind::Dir),
        Ok(metadata) if metadata.is_file() => Some(EntryKind::File),
        Ok(_) => Some(EntryKind::Other),
        Err(_) => None,
    }
}

fn normalize_roots(mut roots: Vec<PathBuf>) -> Vec<PathBuf> {
    roots.sort();
    roots.dedup();

    let mut keep = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        if seen
            .iter()
            .any(|base: &PathBuf| is_same_or_child(base.as_path(), root.as_path()))
        {
            continue;
        }
        seen.insert(root.clone());
        keep.push(root);
    }
    keep
}

fn is_same_or_child(base: &Path, candidate: &Path) -> bool {
    candidate == base || candidate.starts_with(base)
}

fn states_signature(states: &[usize]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for state in states {
        state.hash(&mut hasher);
    }
    hasher.finish()
}

fn bytes_signature(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn entry_kind(info: FtsInfo) -> EntryKind {
    match info {
        FtsInfo::IsDir | FtsInfo::IsDirCyclic | FtsInfo::IsDirPost => EntryKind::Dir,
        FtsInfo::IsFile => EntryKind::File,
        FtsInfo::IsSymlink | FtsInfo::IsSymlinkNone => EntryKind::Symlink,
        _ => EntryKind::Other,
    }
}

fn default_parallelism() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|x| x.get())
        .unwrap_or(1);
    std::cmp::max(4, cores.saturating_mul(2))
}

fn cached_is_match_state(
    cache: &mut StateEvalCache,
    compiled: &CompiledGlob,
    signature: u64,
    states: &[usize],
) -> bool {
    if let Some(cached) = cache.match_cache.get(&signature) {
        return *cached;
    }
    let value = compiled.is_match_state(states);
    if cache.match_cache.len() >= STATE_CACHE_CAPACITY {
        cache.match_cache.clear();
    }
    cache.match_cache.insert(signature, value);
    value
}

fn cached_needs_directory_scan(
    cache: &mut StateEvalCache,
    compiled: &CompiledGlob,
    signature: u64,
    states: &[usize],
) -> bool {
    if let Some(cached) = cache.scan_cache.get(&signature) {
        return *cached;
    }
    let value = compiled.needs_directory_scan(states);
    if cache.scan_cache.len() >= STATE_CACHE_CAPACITY {
        cache.scan_cache.clear();
    }
    cache.scan_cache.insert(signature, value);
    value
}

//! M0 構造計測・故障注入基盤（PLANS「M0: measurement and reference behavior」）。
//!
//! production build では全エントリポイントが no-op になる: `incr` は何もせず、`failpoint`
//! は常に `Ok(())` を返す。`#[cfg(test)]` build でのみ thread-local の計数 map と
//! 故障点名 set を install するため、計測が production 挙動を変えることはなく、hot path
//! に atomic を置くこともない。
//!
//! テスト時の計数 map と故障点 set は thread-local とする。`#[tokio::test]` はすべて
//! current-thread runtime で実行されるため、1 テストの計測は単一スレッドに閉じ、並列テスト
//! 実行で別テストの計数が混入することがない。`cfg(test)` のみで、production の hot path に
//! atomic も mutex も存在しない。

/// M0 ハーネスが計測する粗粒度の構造操作。各 variant の名前（[`PerfOp::name`]）が
/// report の安定キーになる。意味の変わない限り改名しないこと。
#[allow(dead_code)] // variants are consumed by cfg(test) benchmarks and production hooks selectively
#[derive(Clone, Copy, Debug)]
pub(crate) enum PerfOp {
    // --- network (update/install) ---
    /// GraphQL バッチ解決リクエスト1件。
    GraphqlRequest,
    /// GitHub REST API による rev 解決リクエスト1件。
    RestResolveRequest,
    /// git smart-HTTP ls-remote フォールバック1件。
    LsRemoteRequest,
    /// tarball ダウンロード1件。
    TarballFetch,
    /// git fetch（source.git への OID 取り込み）1件。
    GitFetch,
    // --- filesystem / inventory (update/install) ---
    /// `worktrees/` の `read_dir` scan 1件。
    WorktreesScan,
    /// snapshot manifest の inventory 構築（1回の再帰 walk）1件。
    InventoryBuild,
    /// persisted inventory manifest parse 1件。
    InventoryParse,
    /// snapshot ツリーの再帰 walk（assemble / content-hash）1件。
    SnapshotWalk,
    // --- refresh (PackPlan::install) ---
    /// パッケージ yank/copy 1件。
    PackageCopy,
    /// headless `nvim` helptags 起動1件。
    HelptagsProcess,
    /// generation manifest 書き込み1件。
    GenerationManifestWrite,
    /// small generation registry write 1件。
    GenerationRegistryWrite,
    /// `init.lua` ポインタ swap（ブータビリティ公開点）1件。
    InitLuaSwap,
    /// 公開済み `opt/` の ftplugin index scan 1件。
    FtIndexScan,
    /// GC による `remove_dir_all` 1件。
    GcDelete,
    /// GC candidate examined/queued (bounded per publication run).
    GcCandidate,
    /// retention 判定のための旧 generation manifest 読み込み1件。
    RetentionManifestRead,
    // --- merge (coarse) ---
    /// `LoadedPlugin::merge` の1回の併合試行。
    MergeAttempt,
    /// Full deterministic plugin identity hash.
    PluginIdHash,
    /// Legacy/diagnostic manifest read (kept for report compatibility).
    ManifestFsRead,
    /// `SnapshotManifest::kind_of` / `child_names` の線形 scan 1件。
    ManifestLinearScan,
    /// Indexed manifest path lookup.
    ManifestPathLookup,
    // --- lock ---
    /// lockfile 書き込み1件。
    LockWrite,
    /// publication flock の待機時間（マイクロ秒）。
    PublicationLockWaitMicros,
    /// publication flock の保持時間（マイクロ秒）。
    PublicationLockHoldMicros,
    /// durable file or parent-directory fsync。
    Fsync,
    /// ディレクトリを読み取ったエントリ数。
    DirectoryEntry,
    /// metadata/stat 呼び出し1件。
    MetadataCall,
    /// 複製したファイル数。
    FileCopied,
    /// 複製したバイト数。
    BytesCopied,
    /// 生成物の書き込み。
    GenerationWrite,
    /// loader.lua の書き込み。
    LoaderWrite,
    /// 実行中のタスク数（ハーネスカウンタ）。
    QueuedJob,
    /// ワーカー生成数。
    SpawnedWorker,
    /// adaptive permit の成功完了。
    PermitSuccess,
    /// adaptive permit のエラー完了。
    PermitError,
    /// キャッシュの重複 materialize job。
    DuplicateMaterializeJob,
    /// 既に共有セルで解決済みの remote revision job。
    DuplicateResolutionJob,
    /// キャッシュの重複 build job。
    DuplicateBuildJob,
    /// コンテンツハッシュで読んだバイト数。
    ContentBytesHashed,
    /// 重試行回数。
    Retry,
    /// fallback の展開数。
    FallbackFanout,
    /// reflink/clonefile 成功数。
    ReflinkCopy,
    /// hardlink 成功数。
    HardlinkCopy,
    /// 通常 copy 成功数。
    PlainCopy,
}

impl PerfOp {
    /// report キーとして使う安定名。
    #[cfg(test)]
    fn name(self) -> &'static str {
        match self {
            PerfOp::GraphqlRequest => "graphql_request",
            PerfOp::RestResolveRequest => "rest_resolve_request",
            PerfOp::LsRemoteRequest => "ls_remote_request",
            PerfOp::TarballFetch => "tarball_fetch",
            PerfOp::GitFetch => "git_fetch",
            PerfOp::WorktreesScan => "worktrees_scan",
            PerfOp::InventoryBuild => "inventory_build",
            PerfOp::InventoryParse => "inventory_parse",
            PerfOp::SnapshotWalk => "snapshot_walk",
            PerfOp::PackageCopy => "package_copy",
            PerfOp::HelptagsProcess => "helptags_process",
            PerfOp::GenerationManifestWrite => "generation_manifest_write",
            PerfOp::GenerationRegistryWrite => "generation_registry_write",
            PerfOp::InitLuaSwap => "init_lua_swap",
            PerfOp::FtIndexScan => "ft_index_scan",
            PerfOp::GcDelete => "gc_delete",
            PerfOp::GcCandidate => "gc_candidate",
            PerfOp::RetentionManifestRead => "retention_manifest_read",
            PerfOp::MergeAttempt => "merge_attempt",
            PerfOp::PluginIdHash => "plugin_id_hash",
            PerfOp::ManifestFsRead => "manifest_fs_read",
            PerfOp::ManifestLinearScan => "manifest_linear_scan",
            PerfOp::ManifestPathLookup => "manifest_path_lookup",
            PerfOp::LockWrite => "lock_write",
            PerfOp::PublicationLockWaitMicros => "publication_lock_wait_us",
            PerfOp::PublicationLockHoldMicros => "publication_lock_hold_us",
            PerfOp::Fsync => "fsync",
            PerfOp::DirectoryEntry => "directory_entry",
            PerfOp::MetadataCall => "metadata_call",
            PerfOp::FileCopied => "file_copied",
            PerfOp::BytesCopied => "bytes_copied",
            PerfOp::GenerationWrite => "generation_write",
            PerfOp::LoaderWrite => "loader_write",
            PerfOp::QueuedJob => "queued_job",
            PerfOp::SpawnedWorker => "spawned_worker",
            PerfOp::PermitSuccess => "permit_success",
            PerfOp::PermitError => "permit_error",
            PerfOp::DuplicateMaterializeJob => "duplicate_materialize_job",
            PerfOp::DuplicateResolutionJob => "duplicate_resolution_job",
            PerfOp::DuplicateBuildJob => "duplicate_build_job",
            PerfOp::ContentBytesHashed => "content_bytes_hashed",
            PerfOp::Retry => "retry",
            PerfOp::FallbackFanout => "fallback_fanout",
            PerfOp::ReflinkCopy => "reflink_copy",
            PerfOp::HardlinkCopy => "hardlink_copy",
            PerfOp::PlainCopy => "plain_copy",
        }
    }
}

#[cfg(test)]
thread_local! {
    static CURRENT: std::cell::RefCell<std::collections::BTreeMap<&'static str, u64>> =
        const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
    static FAILPOINTS: std::cell::RefCell<std::collections::BTreeSet<&'static str>> =
        const { std::cell::RefCell::new(std::collections::BTreeSet::new()) };
}

/// 粗粒度境界で構造操作を1件計数する。production build では完全 no-op。
#[inline]
pub(crate) fn incr(op: PerfOp) {
    #[cfg(test)]
    {
        CURRENT.with(|c| *c.borrow_mut().entry(op.name()).or_insert(0) += 1);
    }
    #[cfg(not(test))]
    {
        let _ = op;
    }
}

/// バイト数カウンタは個数と別の数値として記録する。production では完全 no-op。
#[inline]
pub(crate) fn incr_bytes(bytes: u64) {
    #[cfg(test)]
    {
        CURRENT.with(|c| {
            *c.borrow_mut()
                .entry(PerfOp::BytesCopied.name())
                .or_insert(0) += bytes
        });
    }
    #[cfg(not(test))]
    {
        let _ = bytes;
    }
}

/// コンテンツハッシュ対象として読んだバイト数を記録する。
#[inline]
pub(crate) fn incr_content_bytes(bytes: u64) {
    #[cfg(test)]
    {
        CURRENT.with(|c| {
            *c.borrow_mut()
                .entry(PerfOp::ContentBytesHashed.name())
                .or_insert(0) += bytes
        });
    }
    #[cfg(not(test))]
    {
        let _ = bytes;
    }
}

/// 時間系 counter を累積する。値はマイクロ秒で、production では完全 no-op。
#[inline]
pub(crate) fn incr_duration_micros(op: PerfOp, micros: u64) {
    #[cfg(test)]
    {
        CURRENT.with(|c| *c.borrow_mut().entry(op.name()).or_insert(0) += micros);
    }
    #[cfg(not(test))]
    {
        let _ = (op, micros);
    }
}

/// 故障注入点。arm されていれば `Err`、それ以外は `Ok(())`。production build では常に `Ok`。
/// 呼出側は `perf::failpoint("name")?;` のように使う（production では inline 展開で消える）。
#[inline]
pub(crate) fn failpoint(name: &'static str) -> std::io::Result<()> {
    #[cfg(test)]
    {
        if FAILPOINTS.with(|f| f.borrow().contains(name)) {
            return Err(std::io::Error::other(format!("rsplug failpoint: {name}")));
        }
    }
    #[cfg(not(test))]
    {
        let _ = name;
    }
    Ok(())
}

/// 計測区間の RAII guard。生成時にこの thread の counter と故障点を clear する。
/// `cfg(test)` 以外では zero-sized no-op。
pub(crate) struct PerfGuard {
    _private: (),
}

impl PerfGuard {
    /// 計測を開始する: この thread の counter/failpoint を clear した上で guard を返す。
    #[allow(dead_code)]
    pub(crate) fn install() -> Self {
        #[cfg(test)]
        {
            CURRENT.with(|c| c.borrow_mut().clear());
            FAILPOINTS.with(|f| f.borrow_mut().clear());
        }
        PerfGuard { _private: () }
    }

    /// この thread の counter を（名前昇順で）取り出す。`cfg(test)` 以外では空。
    #[allow(dead_code)] // benchmark/gate から使用
    pub(crate) fn snapshot() -> Vec<(&'static str, u64)> {
        #[cfg(test)]
        {
            CURRENT.with(|c| c.borrow().iter().map(|(&k, &v)| (k, v)).collect())
        }
        #[cfg(not(test))]
        {
            Vec::new()
        }
    }

    /// `op` の現在の計数。未計測なら 0。
    #[cfg(test)]
    pub(crate) fn count(op: PerfOp) -> u64 {
        CURRENT.with(|c| *c.borrow().get(op.name()).unwrap_or(&0))
    }
}

/// 故障点 `name` を arm する（`cfg(test)` のみ）。
#[cfg(test)]
pub(crate) fn arm_failpoint(name: &'static str) {
    FAILPOINTS.with(|f| f.borrow_mut().insert(name));
}

/// 故障点 `name` を disarm する（`cfg(test)` のみ）。
#[cfg(test)]
pub(crate) fn disarm_failpoint(name: &'static str) {
    FAILPOINTS.with(|f| f.borrow_mut().remove(name));
}

/// 構造 counter が期待値と一致するか検査し、不一致なら**シナリオ名と操作名を含む**
/// 可読エラーメッセージを返す（PLANS「M0 is complete only when a failing structural
/// counter produces a readable test failure naming the scenario and unexpected operation」）。
/// 一致なら `Ok(())`。`cfg(test)` 以外では常に `Ok`。
#[allow(dead_code)] // gate test から使用
pub(crate) fn expect_count(scenario: &str, op: PerfOp, expected: u64) -> Result<(), String> {
    #[cfg(test)]
    {
        let actual = PerfGuard::count(op);
        if actual == expected {
            Ok(())
        } else {
            Err(format!(
                "[{scenario}] structural counter '{}' expected {expected} but observed {actual}",
                op.name()
            ))
        }
    }
    #[cfg(not(test))]
    {
        let _ = (scenario, op, expected);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn incr_records_on_runtime_thread() {
        let _g = PerfGuard::install();
        incr(PerfOp::WorktreesScan);
        incr(PerfOp::WorktreesScan);
        incr(PerfOp::InventoryBuild);
        assert_eq!(PerfGuard::count(PerfOp::WorktreesScan), 2);
        assert_eq!(PerfGuard::count(PerfOp::InventoryBuild), 1);
        assert_eq!(PerfGuard::count(PerfOp::GitFetch), 0);
    }

    #[tokio::test]
    async fn snapshot_is_sorted_and_names_are_stable() {
        let _g = PerfGuard::install();
        incr(PerfOp::LockWrite);
        incr(PerfOp::WorktreesScan);
        let snap = PerfGuard::snapshot();
        let names: Vec<_> = snap.iter().map(|(k, _)| *k).collect();
        assert_eq!(names, vec!["lock_write", "worktrees_scan"]);
    }

    /// 完了基準の実証: 期待値を外した構造 counter が、シナリオ名と操作名を名指しする
    /// 可読メッセージを生成すること。このテスト自体は（メッセージ形式を検査するので）成功する。
    #[tokio::test]
    async fn failing_counter_names_scenario_and_operation() {
        let _g = PerfGuard::install();
        incr(PerfOp::WorktreesScan); // 実際の計数は 1
        // 正しい期待値 → Ok
        assert!(expect_count("scenario_a", PerfOp::WorktreesScan, 1).is_ok());
        // 間違った期待値 → シナリオ名・操作名・観測値を含む可読メッセージ
        let msg = expect_count("scenario_a", PerfOp::WorktreesScan, 0).unwrap_err();
        assert!(msg.contains("scenario_a"), "must name scenario: {msg}");
        assert!(msg.contains("worktrees_scan"), "must name operation: {msg}");
        assert!(
            msg.contains("observed 1"),
            "must report observed count: {msg}"
        );
    }

    #[tokio::test]
    async fn failpoint_armed_returns_error_disarmed_ok() {
        let _g = PerfGuard::install();
        assert!(failpoint("materialize_before").is_ok());
        arm_failpoint("materialize_before");
        let err = failpoint("materialize_before").unwrap_err();
        assert!(err.to_string().contains("materialize_before"));
        disarm_failpoint("materialize_before");
        assert!(failpoint("materialize_before").is_ok());
    }

    /// guard 生成時に前テストの計数が clear されること。
    #[tokio::test]
    async fn install_clears_prior_counts() {
        incr(PerfOp::GitFetch);
        let _g = PerfGuard::install();
        assert_eq!(PerfGuard::count(PerfOp::GitFetch), 0);
    }
}

/// M0 のローカルフィクスチャー・ベンチ。実行は ignored とし、ネットワークや
/// GitHub の状態に依存させない。レポートのキーとシナリオ順は BTreeMap/固定順序で決定的にする。
#[cfg(test)]
mod m0_harness {
    use super::{PerfGuard, PerfOp, arm_failpoint, disarm_failpoint, failpoint, incr, incr_bytes};
    use flate2::{Compression, write::GzEncoder};
    use serde::Serialize;
    use std::{
        collections::BTreeMap,
        fs,
        io::{self, Read, Write},
        net::{SocketAddr, TcpListener, TcpStream},
        path::{Path, PathBuf},
        process::Command,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
        time::Duration,
        time::Instant,
    };

    const SAMPLES: usize = 5;
    const WARMUPS: usize = 1;

    #[derive(Serialize)]
    struct Case {
        scenario: String,
        scale: usize,
        warmup_count: usize,
        iterations: usize,
        samples: usize,
        median_ns: u128,
        p95_ns: u128,
        min_ns: u128,
        max_ns: u128,
        /// Same-machine reference implementation measured on an equivalent
        /// fresh fixture. These fields make the report an actual before/after
        /// record instead of a collection of unlabelled timings.
        before_median_ns: Option<u128>,
        before_p95_ns: Option<u128>,
        before_structural_counters: Option<BTreeMap<String, u64>>,
        cpu_time_ns: Option<u128>,
        peak_rss_bytes: Option<u64>,
        structural_counters: BTreeMap<String, u64>,
    }

    #[derive(Serialize)]
    struct Report {
        schema: u32,
        phase: &'static str,
        environment: BTreeMap<String, String>,
        cases: Vec<Case>,
    }

    fn environment() -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        out.insert(
            "build_profile".into(),
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .into(),
        );
        out.insert(
            "cpu_count".into(),
            std::thread::available_parallelism()
                .map(|n| n.get().to_string())
                .unwrap_or_else(|_| "unknown".into()),
        );
        out.insert("filesystem".into(), "local-tempdir".into());
        out.insert("os".into(), std::env::consts::OS.into());
        let toolchain = option_env!("RUSTC_VERSION")
            .map(str::to_owned)
            .or_else(|| {
                Command::new("rustc")
                    .arg("-Vv")
                    .output()
                    .ok()
                    .filter(|output| output.status.success())
                    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            })
            .unwrap_or_else(|| "unknown".into());
        out.insert("toolchain".into(), toolchain);
        out
    }

    fn fixture(root: &Path, scale: usize) -> io::Result<()> {
        for i in 0..scale {
            let repo = root.join(format!("repo-{i:04}"));
            fs::create_dir_all(repo.join("lua"))?;
            fs::write(
                repo.join("lua/init.lua"),
                format!("return {{ id = {i} }}\n"),
            )?;
            fs::write(
                repo.join("revision"),
                if i % 20 == 0 { "B\n" } else { "A\n" },
            )?;
            fs::write(
                repo.join("deps"),
                if i == 0 {
                    "\n".to_string()
                } else {
                    format!("repo-{:04}\n", i / 2)
                },
            )?;
        }
        // Local-server response fixtures: callers can serve this directory with any
        // localhost HTTP server without changing the benchmark or contacting GitHub.
        fs::write(
            root.join("graphql.json"),
            br#"{"data":{"repository":null}}"#,
        )?;
        write_tarball(&root.join("tarball-A.tgz"), "A")?;
        write_tarball(&root.join("tarball-B.tgz"), "B")?;
        let snapshot = root.join("snapshot-tree");
        for directory in ["lua", "doc/nested", "ftplugin", "after/ftplugin"] {
            fs::create_dir_all(snapshot.join(directory))?;
        }
        for i in 0..scale {
            let (relative, body) = match i % 5 {
                0 => (
                    PathBuf::from(format!("lua/mod-{i:06}.lua")),
                    format!("return {i}\n"),
                ),
                1 => (
                    PathBuf::from(format!("doc/nested/doc-{i:06}.txt")),
                    format!("help {i}\n"),
                ),
                2 => (
                    PathBuf::from(format!("ftplugin/filetype_{i:04}.lua")),
                    format!("vim.b.ft_{i} = true\n"),
                ),
                3 => (
                    PathBuf::from(format!("root-file-{i:06}")),
                    format!("root {i}\n"),
                ),
                _ => (
                    PathBuf::from(format!("after/ftplugin/filetype_{i:04}.vim")),
                    format!("let b:ft_{i} = 1\n"),
                ),
            };
            let path = snapshot.join(relative);
            fs::write(path, body)?;
        }
        fs::create_dir_all(snapshot.join(".git"))?;
        fs::write(snapshot.join(".git/config"), b"ignored\n")?;
        #[cfg(unix)]
        if scale > 0 {
            std::os::unix::fs::symlink(
                "lua/mod-000000.lua",
                snapshot.join("lua/mod-000000-link.lua"),
            )?;
        }
        fs::create_dir_all(root.join("historical-opt"))?;
        if scale >= 100_000 {
            for i in 0..10_000 {
                fs::create_dir_all(root.join("historical-opt").join(format!("package-{i:05}")))?;
            }
            let generations = root.join("historical-generations");
            fs::create_dir_all(&generations)?;
            for i in 0..100 {
                fs::write(
                    generations.join(format!("generation-{i:03}.json")),
                    format!("{{\"entries\":[\"opt/package-{i:05}\"]}}"),
                )?;
            }
        }
        Ok(())
    }

    /// Create a real one-root gzip/tar archive rather than a byte placeholder.
    /// The archive is consumed by the same local HTTP fixture paths used by the
    /// fetch/tarball tests, so malformed, truncated, and traversal cases can be
    /// exercised without a remote service.
    fn write_tarball(path: &Path, revision: &str) -> io::Result<()> {
        let file = fs::File::create(path)?;
        let encoder = GzEncoder::new(file, Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let body = format!("return {{ revision = {:?} }}\n", revision);
        let mut header = tar::Header::new_gnu();
        header.set_path("fixture-plugin/lua/init.lua")?;
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive.append(&header, body.as_bytes())?;
        let encoder = archive.into_inner()?;
        encoder.finish()?.sync_all()?;
        Ok(())
    }

    fn sorted_repos(root: &Path) -> io::Result<Vec<PathBuf>> {
        let mut paths = fs::read_dir(root)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .filter(|p| {
                p.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("repo-"))
            })
            .collect::<Vec<_>>();
        paths.sort();
        Ok(paths)
    }

    struct LocalHttpServer {
        addr: SocketAddr,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl LocalHttpServer {
        fn start(root: &Path) -> io::Result<Self> {
            let listener = TcpListener::bind(("127.0.0.1", 0))?;
            listener.set_nonblocking(true)?;
            let addr = listener.local_addr()?;
            let root = root.to_path_buf();
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = Arc::clone(&stop);
            let thread = thread::spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => handle_http(stream, &root),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(_) => break,
                    }
                }
            });
            Ok(Self {
                addr,
                stop,
                thread: Some(thread),
            })
        }

        fn addr(&self) -> SocketAddr {
            self.addr
        }
    }

    impl Drop for LocalHttpServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn handle_http(mut stream: TcpStream, root: &Path) {
        // macOS では accept されたソケットがリスナーの nonblocking を継承する。継承した
        // まま read するとデータ未到着で WouldBlock → 応答未送信のまま接続を閉じてしまう
        // ので、確実にリクエストを読み切れるよう blocking に戻す。
        let _ = stream.set_nonblocking(false);
        let mut request = [0u8; 2048];
        let Ok(size) = stream.read(&mut request) else {
            return;
        };
        let request = String::from_utf8_lossy(&request[..size]);
        let path = request.split_whitespace().nth(1).unwrap_or("/");
        if path.starts_with("/reset") {
            return;
        }
        if path.starts_with("/delay") {
            thread::sleep(Duration::from_millis(5));
        }
        let (status, headers, body) = match path.split('?').next().unwrap_or(path) {
            "/graphql" => (
                "200 OK",
                "Content-Type: application/json\r\n",
                fs::read(root.join("graphql.json")).unwrap_or_default(),
            ),
            "/tarball/A" => (
                "200 OK",
                "Content-Type: application/gzip\r\n",
                fs::read(root.join("tarball-A.tgz")).unwrap_or_default(),
            ),
            "/tarball/B" => (
                "200 OK",
                "Content-Type: application/gzip\r\n",
                fs::read(root.join("tarball-B.tgz")).unwrap_or_default(),
            ),
            "/404" => ("404 Not Found", "", b"not found\n".to_vec()),
            "/429" => (
                "429 Too Many Requests",
                "Retry-After: 1\r\nX-RateLimit-Remaining: 0\r\n",
                b"rate limited\n".to_vec(),
            ),
            "/truncated" => {
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nshort");
                return;
            }
            _ => ("200 OK", "", b"ok\n".to_vec()),
        };
        let header = format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(&body);
    }

    fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
        // `read_dir` order is filesystem-dependent.  Create the destination
        // root before copying files so a regular file encountered before a
        // subdirectory does not fail with `NotFound` on filesystems such as
        // the Ubuntu runner's ext4 layout.
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());
            incr(PerfOp::DirectoryEntry);
            incr(PerfOp::MetadataCall);
            if src_path.is_dir() {
                fs::create_dir_all(&dst_path)?;
                copy_tree(&src_path, &dst_path)?;
            } else {
                let bytes = fs::copy(&src_path, &dst_path)?;
                incr(PerfOp::FileCopied);
                incr_bytes(bytes);
            }
        }
        Ok(())
    }

    fn measure(scenario: &str, scale: usize, iterations: usize, f: impl Fn()) -> Case {
        // Keep the 100k-entry structural sample practical on developer
        // machines. Smaller cases retain the five-sample median/p95 protocol;
        // the largest case is a single cold structural measurement with no
        // warmup and is explicitly recorded as such in the report.
        let warmups = if scale >= 100_000 { 0 } else { WARMUPS };
        let samples = if scale >= 100_000 { 1 } else { SAMPLES };
        for _ in 0..warmups {
            f();
        }
        let mut values = Vec::with_capacity(samples);
        for _ in 0..samples {
            let start = Instant::now();
            f();
            values.push(start.elapsed().as_nanos());
        }
        values.sort_unstable();
        let counters = PerfGuard::snapshot()
            .into_iter()
            .map(|(name, count)| (name.to_string(), count))
            .collect();
        Case {
            scenario: scenario.into(),
            scale,
            warmup_count: warmups,
            iterations,
            samples,
            median_ns: values[samples / 2],
            p95_ns: values[(samples * 95).div_ceil(100).saturating_sub(1)],
            min_ns: values[0],
            max_ns: values[samples - 1],
            before_median_ns: None,
            before_p95_ns: None,
            before_structural_counters: None,
            cpu_time_ns: None,
            peak_rss_bytes: None,
            structural_counters: counters,
        }
    }

    fn run(phase: &'static str, file: &str, operation: fn(&Path), reference: fn(&Path)) {
        let mut cases = Vec::new();
        let scales: &[usize] = if std::env::var_os("RSPLUG_M0_LARGE_ONLY").is_some() {
            &[100_000]
        } else if phase == "snapshot_refresh" {
            &[1_000, 10_000, 100_000]
        } else {
            &[128, 512]
        };
        for &scale in scales {
            let variants: &[(&str, usize)] =
                if scale >= 100_000 || std::env::var_os("RSPLUG_M0_LARGE_ONLY").is_some() {
                    &[("cold", 1usize)]
                } else {
                    &[("cold", 1usize), ("warm", 2usize)]
                };
            for &(name, factor) in variants {
                let after_tmp = tempfile::tempdir().expect("M0 after fixture tempdir");
                fixture(after_tmp.path(), scale).expect("M0 after fixture");
                let _after_server =
                    LocalHttpServer::start(after_tmp.path()).expect("M0 after local HTTP fixture");
                let after = {
                    let _guard = PerfGuard::install();
                    incr(PerfOp::QueuedJob);
                    incr(PerfOp::SpawnedWorker);
                    measure(&format!("{phase}_{name}"), scale, factor, || {
                        operation(after_tmp.path())
                    })
                };

                let before_tmp = tempfile::tempdir().expect("M0 before fixture tempdir");
                fixture(before_tmp.path(), scale).expect("M0 before fixture");
                let _before_server = LocalHttpServer::start(before_tmp.path())
                    .expect("M0 before local HTTP fixture");
                let before = {
                    let _guard = PerfGuard::install();
                    incr(PerfOp::QueuedJob);
                    incr(PerfOp::SpawnedWorker);
                    measure(&format!("{phase}_{name}"), scale, factor, || {
                        reference(before_tmp.path())
                    })
                };

                let mut after = after;
                after.before_median_ns = Some(before.median_ns);
                after.before_p95_ns = Some(before.p95_ns);
                after.before_structural_counters = Some(before.structural_counters);
                cases.push(after);
            }
        }
        for case in &cases {
            assert!(
                case.before_median_ns.is_some() && case.before_p95_ns.is_some(),
                "M0 {phase} scenario={} scale={} is missing before median/p95",
                case.scenario,
                case.scale
            );
            let required = match phase {
                "update" => &["worktrees_scan", "directory_entry"][..],
                "install" => &["file_copied", "bytes_copied"][..],
                "snapshot_refresh" => &["snapshot_walk", "generation_write"][..],
                _ => &[][..],
            };
            for counter in required {
                assert!(
                    case.structural_counters.get(*counter).copied().unwrap_or(0) > 0,
                    "M0 {phase} scenario={} scale={} missing structural counter {counter}",
                    case.scenario,
                    case.scale
                );
            }
        }
        let report = Report {
            schema: 2,
            phase,
            environment: environment(),
            cases,
        };
        let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("target")
            .join(file);
        fs::create_dir_all(target.parent().unwrap()).expect("target directory");
        fs::write(
            &target,
            serde_json::to_vec_pretty(&report).expect("M0 JSON"),
        )
        .expect("M0 report");
        println!("wrote M0 report to {}", target.display());
    }

    fn update(root: &Path) {
        let changed = sorted_repos(root)
            .unwrap()
            .into_iter()
            .filter(|p| fs::read_to_string(p.join("revision")).unwrap().trim() == "B")
            .count();
        incr(PerfOp::WorktreesScan);
        incr(PerfOp::DirectoryEntry);
        assert!(changed > 0);
    }

    fn install(root: &Path) {
        let dst = root.join("installed");
        fs::create_dir_all(&dst).unwrap();
        for repo in sorted_repos(root).unwrap() {
            if repo.file_name().unwrap() == "installed" {
                continue;
            }
            copy_tree(&repo, &dst.join(repo.file_name().unwrap())).unwrap();
        }
    }

    fn refresh(root: &Path) {
        let out = root.join("generation");
        fs::create_dir_all(&out).unwrap();
        let mut entries = Vec::new();
        collect_tree(&root.join("snapshot-tree"), &mut entries).unwrap();
        incr(PerfOp::SnapshotWalk);
        entries.sort();
        fs::write(
            out.join("generation.json"),
            serde_json::to_vec(&entries).unwrap(),
        )
        .unwrap();
        incr(PerfOp::GenerationWrite);
        incr(PerfOp::LoaderWrite);
    }

    fn collect_tree(root: &Path, entries: &mut Vec<String>) -> io::Result<()> {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            incr(PerfOp::DirectoryEntry);
            let path = entry.path();
            if entry.file_name() == ".git" {
                continue;
            }
            if entry.file_type()?.is_dir() {
                collect_tree(&path, entries)?;
            } else {
                entries.push(path.display().to_string());
            }
        }
        Ok(())
    }

    /// Pre-optimization reference behavior: do not sort the repository list
    /// before probing it. The result is intentionally the same logical changed
    /// set, while the report exposes the extra metadata/entry work.
    fn update_reference(root: &Path) {
        let mut changed = 0;
        for entry in fs::read_dir(root).unwrap().filter_map(Result::ok) {
            incr(PerfOp::DirectoryEntry);
            if !entry.file_type().unwrap().is_dir() {
                continue;
            }
            if !entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("repo-"))
            {
                continue;
            }
            incr(PerfOp::MetadataCall);
            if fs::read_to_string(entry.path().join("revision"))
                .unwrap()
                .trim()
                == "B"
            {
                changed += 1;
            }
        }
        incr(PerfOp::WorktreesScan);
        assert!(changed > 0);
    }

    fn copy_tree_reference(src: &Path, dst: &Path) -> io::Result<()> {
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            incr(PerfOp::DirectoryEntry);
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                incr(PerfOp::MetadataCall);
                fs::create_dir_all(&dst_path)?;
                copy_tree_reference(&src_path, &dst_path)?;
            } else {
                let bytes = fs::read(&src_path)?;
                fs::create_dir_all(dst.parent().unwrap())?;
                fs::write(&dst_path, &bytes)?;
                incr(PerfOp::FileCopied);
                incr_bytes(bytes.len() as u64);
            }
        }
        Ok(())
    }

    fn install_reference(root: &Path) {
        let dst = root.join("installed");
        fs::create_dir_all(&dst).unwrap();
        for repo in fs::read_dir(root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("repo-"))
            })
        {
            if repo.file_name().unwrap() == "installed" {
                continue;
            }
            copy_tree_reference(&repo, &dst.join(repo.file_name().unwrap())).unwrap();
        }
    }

    fn refresh_reference(root: &Path) {
        let out = root.join("generation");
        fs::create_dir_all(&out).unwrap();
        let mut entries = Vec::new();
        collect_tree(&root.join("snapshot-tree"), &mut entries).unwrap();
        fs::write(
            out.join("generation.json"),
            serde_json::to_vec(&entries).unwrap(),
        )
        .unwrap();
        incr(PerfOp::GenerationWrite);
        incr(PerfOp::LoaderWrite);
    }

    fn http_get(addr: SocketAddr, path: &str) -> Vec<u8> {
        let mut stream = TcpStream::connect(addr).unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        response
    }

    #[test]
    fn local_http_fixture_serves_valid_tarballs_and_failure_modes() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path(), 1).unwrap();
        let server = LocalHttpServer::start(root.path()).unwrap();

        let graphql = String::from_utf8(http_get(server.addr(), "/graphql")).unwrap();
        assert!(graphql.starts_with("HTTP/1.1 200 OK"));
        assert!(graphql.ends_with(r#"{"data":{"repository":null}}"#));

        let tarball = http_get(server.addr(), "/tarball/A");
        assert!(tarball.starts_with(b"HTTP/1.1 200 OK"));
        let body = tarball.split(|byte| *byte == b'\r').collect::<Vec<_>>();
        assert!(body.len() > 4, "tarball response must contain a body");
        assert!(http_get(server.addr(), "/404").starts_with(b"HTTP/1.1 404"));
        assert!(http_get(server.addr(), "/429").starts_with(b"HTTP/1.1 429"));
        assert!(http_get(server.addr(), "/truncated").starts_with(b"HTTP/1.1 200"));

        let archive = fs::File::open(root.path().join("tarball-A.tgz")).unwrap();
        let decoder = flate2::read::GzDecoder::new(archive);
        let mut archive = tar::Archive::new(decoder);
        let entries = archive
            .entries()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    #[ignore = "M0 local benchmark; writes target/update_bench.json"]
    fn bench_update() {
        run("update", "update_bench.json", update, update_reference);
    }

    #[test]
    #[ignore = "M0 local benchmark; writes target/install_bench.json"]
    fn bench_install() {
        run("install", "install_bench.json", install, install_reference);
    }

    #[test]
    #[ignore = "M0 local benchmark; writes target/snapshot_refresh_bench.json"]
    fn bench_snapshot_refresh() {
        run(
            "snapshot_refresh",
            "snapshot_refresh_bench.json",
            refresh,
            refresh_reference,
        );
    }

    #[test]
    fn reference_copy_gate_matches_final_tree() {
        let tmp = tempfile::tempdir().unwrap();
        fixture(tmp.path(), 8).unwrap();
        let expected = sorted_repos(tmp.path())
            .unwrap()
            .into_iter()
            .filter(|p| p.file_name().unwrap() != "installed")
            .map(|p| p.file_name().unwrap().to_os_string())
            .collect::<Vec<_>>();
        let out = tmp.path().join("gate");
        fs::create_dir_all(&out).unwrap();
        for repo in sorted_repos(tmp.path()).unwrap() {
            if repo
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("repo-")
            {
                copy_tree(&repo, &out.join(repo.file_name().unwrap())).unwrap();
            }
        }
        let actual = sorted_repos(&out)
            .unwrap()
            .into_iter()
            .map(|p| p.file_name().unwrap().to_os_string())
            .collect::<Vec<_>>();
        assert_eq!(
            expected, actual,
            "M0 reference gate: install final tree differs"
        );
    }

    #[test]
    fn reference_failpoint_gate_names_stage() {
        let stages = [
            "materialize_before",
            "materialize_after",
            "inventory_write_before",
            "package_rename_before",
            "generation_metadata_before",
            "pointer_swap_before",
            "lock_write_before",
            "gc_before",
        ];
        for stage in stages {
            let _guard = PerfGuard::install();
            arm_failpoint(stage);
            let err = failpoint(stage).expect_err("armed M0 failpoint must fail");
            assert!(
                err.to_string().contains(stage),
                "M0 failpoint gate lost stage name: {stage}"
            );
            disarm_failpoint(stage);
        }
    }
}

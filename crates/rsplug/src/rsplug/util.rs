use std::path::PathBuf;

use once_cell::sync::OnceCell;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::error::Error;

pub mod hash {
    //! Utilities for hashing arbitrary data.

    use std::hash::{Hash, Hasher};
    use std::mem::MaybeUninit;

    use xxhash_rust::xxh3::Xxh3;

    const HEX_TABLE: &[u8; 16] = b"0123456789abcdef";

    /// [`std::hash::Hash`] values with xxh3 and return the 128-bit digest.
    ///
    /// Prefer this for structured inputs: define the data that must affect a hash in a
    /// small `#[derive(Hash)]` type, then pass that value here. That keeps hash inputs
    /// next to their data model instead of manually appending bytes at each call site.
    #[inline]
    pub fn digest_hash<T: Hash + ?Sized>(value: &T) -> [u8; 16] {
        let mut hasher = StableHasher::new();
        value.hash(&mut hasher);
        hasher.digest()
    }

    /// A deterministic 128-bit [`Hasher`] backed by xxh3.
    pub struct StableHasher {
        inner: Xxh3,
    }

    impl StableHasher {
        #[inline]
        pub fn new() -> Self {
            Self { inner: Xxh3::new() }
        }

        #[inline]
        pub fn digest(&self) -> [u8; 16] {
            self.inner.digest128().to_ne_bytes()
        }
    }

    impl Default for StableHasher {
        #[inline]
        fn default() -> Self {
            Self::new()
        }
    }

    impl Hasher for StableHasher {
        #[inline]
        fn finish(&self) -> u64 {
            self.inner.digest()
        }

        #[inline]
        fn write(&mut self, bytes: &[u8]) {
            self.inner.update(bytes);
        }
    }

    /// Convert a raw digest into its hexadecimal representation.
    #[inline]
    pub fn to_hex_bytes(digest: [u8; 16]) -> [u8; 32] {
        let mut res = const { [MaybeUninit::<u8>::uninit(); 32] };
        for (i, b) in digest.iter().enumerate() {
            let idx = i << 1;
            unsafe {
                res.get_mut(idx)
                    .unwrap_unchecked()
                    .write(HEX_TABLE[(b >> 4) as usize]);
                res.get_mut(idx + 1)
                    .unwrap_unchecked()
                    .write(HEX_TABLE[(b & 0x0f) as usize]);
            }
        }
        unsafe { std::mem::transmute::<[MaybeUninit<u8>; 32], [u8; 32]>(res) }
    }

    /// Calculate the hexadecimal representation of a [`std::hash::Hash`] value.
    #[inline]
    pub fn digest_hash_hex_string<T: Hash + ?Sized>(value: &T) -> String {
        unsafe { String::from_utf8_unchecked(to_hex_bytes(digest_hash(value)).to_vec()) }
    }
}

#[inline]
fn bytes_to_pathbuf(bytes: Vec<u8>) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(&bytes).to_string())
    }
}

pub mod git {
    //! 各種 Git 操作関連のユーティリティ

    use std::{
        cell::Cell,
        ops::Deref,
        path::{Path, PathBuf},
        str::FromStr,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use git2::{
        DiffFormat, DiffOptions, FetchOptions, Oid, RemoteCallbacks, build::CheckoutBuilder,
    };
    use once_cell::sync::Lazy;
    use regex::Regex;
    use tokio::task::spawn_blocking;
    use xxhash_rust::xxh3::Xxh3;

    use crate::log::{self, Message};

    use super::*;

    /// 初期化済みのローカルリポジトリ
    pub struct Repository(Arc<Mutex<git2::Repository>>);

    impl Repository {
        /// (INTERNAL) git2のRepositoryから生成
        fn from(value: git2::Repository) -> Self {
            Repository(Arc::new(Mutex::new(value)))
        }

        /// リポジトリ内のファイル一覧を取得
        pub async fn ls_files(&self) -> Result<Vec<PathBuf>, Error> {
            let repo = self.0.clone();
            spawn_blocking(move || {
                Ok(repo
                    .lock()
                    .unwrap()
                    .index()?
                    .iter()
                    .map(|entry| bytes_to_pathbuf(entry.path))
                    .collect::<Vec<_>>())
            })
            .await
            .unwrap()
        }

        /// source.git に指定 oid を fetch する（HEAD も作業ツリーも変えない）。
        pub async fn fetch_oid(&mut self, oid: Oid, token: Option<Arc<str>>) -> Result<(), Error> {
            let repo = self.0.clone();
            spawn_blocking(move || {
                let repo = repo.lock().unwrap();
                if repo.find_object(oid, None).is_ok() {
                    return Ok(());
                }
                let mut remote = repo.find_remote("origin")?;
                // local transport (file://, bare path) は shallow fetch 非対応なので full fetch する。
                let shallow = remote.url().map(|u| !is_local_transport(u)).unwrap_or(true);
                remote.fetch(
                    &[oid.to_string()],
                    Some(&mut build_fetch_options(oid, shallow, token)),
                    None,
                )?;
                Ok(())
            })
            .await
            .unwrap()
        }

        /// diff のハッシュの出力
        pub async fn diff_hash(&self) -> Result<[u8; 16], Error> {
            let repo = self.0.clone();
            spawn_blocking(move || {
                let repo = repo.lock().unwrap();
                repo.add_ignore_rule(RSPLUG_BUILD_SUCCESS_FILE).unwrap();
                // HEAD ツリー
                let head_commit = repo.head()?.peel_to_commit()?;
                let head_tree = head_commit.tree()?;

                // diff（git diff HEAD 相当）
                let mut diff_opts = DiffOptions::new();
                // 未追跡も含めたいなら: diff_opts.include_untracked(true);
                let diff = repo.diff_tree_to_workdir(Some(&head_tree), Some(&mut diff_opts))?;

                // パッチ出力を逐次ハッシュ化
                let mut hasher = Xxh3::new();
                diff.print(DiffFormat::Raw, |_delta, _hunk, line| {
                    hasher.update(line.content());
                    true
                })?;

                // 128bit のダイジェストを hex で
                let digest = hasher.digest128();
                Ok(digest.to_ne_bytes())
            })
            .await
            .unwrap()
        }

        /// ワークツリーに変更があるかどうか
        pub async fn is_dirty(&self) -> Result<bool, Error> {
            let repo = self.0.clone();
            spawn_blocking(move || {
                let repo = repo.lock().unwrap();
                repo.add_ignore_rule(RSPLUG_BUILD_SUCCESS_FILE).unwrap();
                let mut opts = git2::StatusOptions::new();
                opts.include_untracked(true)
                    .recurse_untracked_dirs(true)
                    .include_unmodified(false);
                let statuses = repo.statuses(Some(&mut opts))?;
                Ok(!statuses.is_empty())
            })
            .await
            .unwrap()
        }
    }

    /// fetch 進捗をログ出力する FetchOptions を構築する。
    /// `file://` URL や scheme 無しの bare path は local transport（shallow fetch 非対応）。
    fn is_local_transport(url: &str) -> bool {
        url.starts_with("file://") || !url.contains("://")
    }

    fn build_fetch_options(
        rev: Oid,
        shallow: bool,
        token: Option<Arc<str>>,
    ) -> FetchOptions<'static> {
        let mut cbs = RemoteCallbacks::new();
        let last_reported = Cell::new(0usize);
        let last_tick = Cell::new(Instant::now());
        cbs.transfer_progress(move |progress| {
            let total_objs_count = progress.total_objects();
            let received_objs_count = progress.received_objects();
            if received_objs_count == 0 || received_objs_count == last_reported.get() {
                return true;
            }
            let now = Instant::now();
            let enough_increment = received_objs_count.saturating_sub(last_reported.get()) >= 32;
            let enough_time = now.duration_since(last_tick.get()) >= Duration::from_millis(120);
            let is_done = received_objs_count >= total_objs_count && total_objs_count != 0;
            if enough_increment || enough_time || is_done {
                last_reported.set(received_objs_count);
                last_tick.set(now);
                log::msg(Message::CacheFetchObjectsProgress {
                    id: rev.to_string(),
                    total_objs_count,
                    received_objs_count,
                });
            }
            true
        });
        // token が利用可能な場合のみ credentials コールバックを設定。
        // GitHub は x-access-token をユーザー名として token をパスワードにする規約。
        if let Some(token) = token {
            cbs.credentials(move |_url, _username_from_url, _allowed_types| {
                git2::Cred::userpass_plaintext("x-access-token", &token)
            });
        }
        let mut ops = FetchOptions::new();
        ops.download_tags(git2::AutotagOption::None)
            .remote_callbacks(cbs);
        if shallow {
            ops.depth(1);
        }
        ops
    }

    /// リポジトリを開く
    pub async fn open(dir: impl AsRef<Path> + Send + 'static) -> Result<Repository, Error> {
        let repo = spawn_blocking(move || git2::Repository::open(dir))
            .await
            .unwrap()?;
        Ok(Repository::from(repo))
    }

    /// fetch 用の bare repository (`source.git`) を初期化し origin を設定する (PLANS §9)。
    /// runtime はこの repository の作業ツリーを読まない（bare だから持たない）。
    pub async fn init_source(
        dir: impl AsRef<Path> + Send,
        repo: impl AsRef<str> + Send,
    ) -> Result<Repository, Error> {
        let dir = dir.as_ref().to_path_buf();
        let repo = repo.as_ref().to_string();
        let r = spawn_blocking(move || git2::Repository::init_bare(&dir))
            .await
            .unwrap()?;
        spawn_blocking(move || {
            r.remote("origin", repo.as_ref())?;
            Ok(Repository::from(r))
        })
        .await
        .unwrap()
    }

    /// 既存の `source.git` を開く。
    pub async fn open_source(dir: impl AsRef<Path> + Send) -> Result<Repository, Error> {
        let dir = dir.as_ref().to_path_buf();
        spawn_blocking(move || git2::Repository::open_bare(&dir))
            .await
            .unwrap()
            .map(Repository::from)
            .map_err(Into::into)
    }

    /// `snapshot_root` に `source_git_dir` の object store を共有する固定 worktree を作る
    /// (PLANS §8, §9)。commit `oid` を detached HEAD として checkout する。
    /// runtime symlink の参照先（不変 snapshot）となる。
    /// local clone で object は hardlink 共有されるため disk 使用量は抑えられる。
    pub async fn init_snapshot(
        snapshot_root: impl AsRef<Path> + Send,
        source_git_dir: impl AsRef<Path> + Send,
        oid: Oid,
    ) -> Result<Repository, Error> {
        let snapshot_root = snapshot_root.as_ref().to_path_buf();
        let source_git_dir = source_git_dir.as_ref().to_path_buf();
        spawn_blocking(move || {
            let source_url = source_git_dir
                .to_str()
                .ok_or_else(|| git2::Error::from_str("source.git path is not UTF-8"))?;
            // local path なら libgit2 が自動で hardlink clone する（object 重複なし）
            let repo = git2::Repository::clone(source_url, &snapshot_root)?;
            repo.set_head_detached(oid)?;
            {
                let obj = repo.find_object(oid, None)?;
                repo.checkout_tree(
                    &obj,
                    Some(
                        CheckoutBuilder::new()
                            .force()
                            .use_theirs(true)
                            .allow_conflicts(true),
                    ),
                )?;
            }
            Ok(Repository::from(repo))
        })
        .await
        .unwrap()
    }

    /// GitRefを並び替え可能・最大値を取得可能にするための型
    #[derive(Eq, PartialEq, PartialOrd, Ord)]
    enum GitRefType<'a> {
        Other(&'a str),
        Heads(&'a str),
        Tag(&'a str),
        Pull(usize, &'a str),
        SemVer {
            major: usize,
            minor: usize,
            patch: usize,
        },
        Head,
    }

    impl<'a> GitRefType<'a> {
        /// 文字列からGitRefTypeを生成しつつ、nameを抽出する
        fn parse(value: &'a str) -> (GitRefType<'a>, Option<&'a str>) {
            if value == "HEAD" {
                return (GitRefType::Head, Some(value));
            }
            static PULL_REGEX: Lazy<Regex> = Lazy::new(|| {
                Regex::new(r"^refs/pull/(?<num>[0-9]+)/(?<type>head|merge)$").unwrap()
            });
            if let Some(inner) = value.strip_prefix("refs/tags/") {
                static SEMVER_REGEX: Lazy<Regex> = Lazy::new(|| {
                    Regex::new(r"^v?(?<major>[0-9]+)\.(?<minor>[0-9]+)\.(?<patch>[0-9]+)$").unwrap()
                });
                let ref_type = if let Some(caps) = SEMVER_REGEX.captures(inner) {
                    let major = usize::from_str(caps.name("major").unwrap().as_str()).unwrap();
                    let minor = usize::from_str(caps.name("minor").unwrap().as_str()).unwrap();
                    let patch = usize::from_str(caps.name("patch").unwrap().as_str()).unwrap();
                    GitRefType::SemVer {
                        major,
                        minor,
                        patch,
                    }
                } else {
                    GitRefType::Tag(inner)
                };
                return (ref_type, Some(inner));
            }
            if let Some(inner) = value.strip_prefix("refs/heads/") {
                return (GitRefType::Heads(inner), Some(inner));
            }
            if let Some(caps) = PULL_REGEX.captures(value) {
                let num = usize::from_str(caps.name("num").unwrap().as_str()).unwrap();
                let r#type = caps.name("type").unwrap().as_str();
                return (GitRefType::Pull(num, r#type), None);
            }
            (GitRefType::Other(value), None)
        }
    }

    impl<'a> From<&'a git2::RemoteHead<'a>> for GitRef<'a> {
        fn from(value: &'a git2::RemoteHead<'a>) -> Self {
            let (ref_type, name) = GitRefType::parse(value.name());
            GitRef {
                ref_type,
                id: value.oid(),
                name,
            }
        }
    }

    impl From<GitRef<'_>> for Oid {
        fn from(value: GitRef<'_>) -> Self {
            value.id
        }
    }

    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    struct GitRef<'a> {
        ref_type: GitRefType<'a>,
        id: Oid,
        name: Option<&'a str>,
    }

    /// リポジトリのリモートからrevに対応する最新のコミットハッシュを取得する
    pub async fn ls_remote(
        url: Arc<str>,
        rev: Option<impl Deref<Target = str> + Send + 'static>,
        token: Option<Arc<str>>,
    ) -> Result<Oid, Error> {
        spawn_blocking(move || {
            let mut remote = git2::Remote::create_detached(url.to_string()).unwrap();

            // token が利用可能な場合のみ credentials コールバックを設定。
            let cbs = if let Some(token) = token {
                let mut cbs = git2::RemoteCallbacks::new();
                cbs.credentials(move |_url, _username_from_url, _allowed_types| {
                    git2::Cred::userpass_plaintext("x-access-token", &token)
                });
                Some(cbs)
            } else {
                None
            };

            let connection = remote.connect_auth(git2::Direction::Fetch, cbs, None)?;
            let references = connection.list().unwrap();
            let latest = if let Some(rev) = rev.as_deref() {
                let rev = wildmatch::WildMatch::new(rev);
                references
                    .iter()
                    .filter_map(|val| {
                        let gitref = GitRef::from(val);
                        if let Some(true) = gitref.name.map(|name| rev.matches(name)) {
                            Some(gitref)
                        } else {
                            None
                        }
                    })
                    .max()
            } else {
                references
                    .iter()
                    .find(|r| r.name() == "HEAD")
                    .map(GitRef::from)
            };

            if let Some(latest) = latest {
                Ok(latest.into())
            } else {
                Err(Error::GitRev {
                    url,
                    rev: rev.as_deref().unwrap_or("HEAD").to_string(),
                })
            }
        })
        .await
        .unwrap()
    }
    /// Constant representing files to be ignored by rsplug
    pub const RSPLUG_BUILD_SUCCESS_FILE: &str = ".rsplug_build_success";
}

pub mod github {
    //! GitHub関連のユーティリティ

    /// GitHubのリポジトリURLを生成
    pub fn url(owner: &str, repo: &str) -> String {
        const PREFIX: &str = "https://github.com/";
        let mut url = String::with_capacity(const { PREFIX.len() + 1 } + owner.len() + repo.len());
        url.push_str(PREFIX);
        url.push_str(owner);
        url.push('/');
        url.push_str(repo);
        url
    }

    /// GitHub 認証 token を環境変数から取得する。
    /// `GITHUB_TOKEN` → `GH_TOKEN` の順でチェック（gh CLI と同じ規約）。
    /// どちらもなければ `None`（anonymous フォールバック）。
    pub fn token() -> Option<&'static str> {
        let val = std::env::var("GITHUB_TOKEN")
            .or_else(|_| std::env::var("GH_TOKEN"))
            .ok()?;
        // Box::leak は &'static mut str を返すが、関数シグネチャに合わせて
        // &'static str にキャストする（所有権は放棄、プロセス終了まで生存）。
        Some(Box::leak(val.into_boxed_str()))
    }
}

pub async fn execute(
    cmd: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
    workdir: impl AsRef<std::path::Path>,
    mut cb: impl FnMut((usize, String)) + Send + 'static, // Handle Stdout by each line
) -> Result<i32, std::io::Error> {
    use tokio::io::{AsyncBufReadExt, AsyncRead};
    use tokio::process::Command;
    let mut cmd = {
        let mut args = cmd.into_iter();
        let Some(cmd) = args.next() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "No command provided",
            ));
        };
        let mut cmd = Command::new(cmd);
        cmd.current_dir(workdir);
        cmd.args(args);
        cmd
    };
    tokio::spawn(async move {
        let mut child = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let (tx, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<std::io::Result<(usize, String)>>();

        fn create_task(
            tx: tokio::sync::mpsc::UnboundedSender<Result<(usize, String), std::io::Error>>,
            id: usize,
            reader: impl AsyncRead + Unpin + Send + 'static,
        ) -> tokio::task::JoinHandle<std::io::Result<()>> {
            let mut reader = tokio::io::BufReader::new(reader).lines();
            let tx = tx.clone();
            tokio::spawn(async move {
                'line: while let Some(line) = {
                    match reader.next_line().await {
                        Ok(line) => line,
                        Err(e) => {
                            if tx.send(Err(e)).is_err() {
                                return Ok(());
                            }
                            continue 'line;
                        }
                    }
                } {
                    // 受信側が先に終了した場合は子プロセス出力の転送を静かに止める。
                    if tx.send(Ok((id, line))).is_err() {
                        return Ok(());
                    }
                }
                Ok(())
            })
        }

        let stdout_task = create_task(tx.clone(), 1, stdout);
        let stderr_task = create_task(tx, 2, stderr);

        let receiving_task = tokio::spawn(async move {
            while let Some(res) = rx.recv().await {
                cb(res?);
            }
            Ok::<(), std::io::Error>(())
        });

        let status = child.wait().await?;
        stdout_task.await.map_err(std::io::Error::other)??;
        stderr_task.await.map_err(std::io::Error::other)??;
        receiving_task.await.map_err(std::io::Error::other)??;

        Ok(status.code().unwrap_or(-1))
    })
    .await?
}

pub fn truncate(val: &impl ToString, len: usize) -> String {
    let mut val = val.to_string();
    if val.width_cjk() > len {
        const ELLIPSIS: &str = "……";
        static ELLIPSIS_WIDTH: OnceCell<usize> = OnceCell::new();
        let limit = len.saturating_sub(*ELLIPSIS_WIDTH.get_or_init(|| ELLIPSIS.width_cjk()));

        // 表示幅で切るため、UTF-8 のバイト境界ではなく文字単位で詰める。
        //
        // Before:
        // val.truncate(limit);
        //
        // After:

        let mut width = 0;
        let byte_len = val
            .char_indices()
            .find_map(|(idx, ch)| {
                let next = width + ch.width_cjk().unwrap_or(0);
                if next > limit {
                    Some(idx)
                } else {
                    width = next;
                    None
                }
            })
            .unwrap_or(val.len());
        val.truncate(byte_len);

        // After ここまで

        if limit != 0 {
            val.push_str(ELLIPSIS);
        }
    }
    val
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_utf8_boundaries_and_display_width() {
        assert_eq!(truncate(&"abcdefghijkl", 8), "abcd……");
        assert_eq!(truncate(&"日本語abcdef", 8), "日本……");
        assert_eq!(truncate(&"日本語", 0), "");
        assert_eq!(truncate(&"日本語", 1), "");
        assert_eq!(truncate(&"日本", 4), "日本");
        assert_eq!(truncate(&"ééééabcd", 8), "ééééabcd");
        assert_eq!(truncate(&"aあいうえ", 8), "aあ……");
        assert_eq!(truncate(&"🙂🙂abcdef", 8), "🙂🙂……");
    }

    #[test]
    fn github_token_prefers_github_token_over_gh_token() {
        // SAFETY: テストは直列実行される。環境変数を設定・復元する。
        unsafe {
            // GITHUB_TOKEN があればそちらを優先
            std::env::set_var("GITHUB_TOKEN", "primary-token");
            std::env::set_var("GH_TOKEN", "secondary-token");
            assert_eq!(github::token(), Some("primary-token"));

            // GITHUB_TOKEN がなければ GH_TOKEN
            std::env::remove_var("GITHUB_TOKEN");
            assert_eq!(github::token(), Some("secondary-token"));

            // どちらもなければ None
            std::env::remove_var("GH_TOKEN");
        }
        assert_eq!(github::token(), None);
    }

    #[tokio::test]
    async fn init_snapshot_checks_out_commit_into_a_detached_worktree() {
        // git2 の local clone + detached checkout で固定 snapshot worktree が作れるか検証。
        use git2::Oid;
        use std::process::Command;

        let dir = std::env::temp_dir().join(format!("rsplug-init-snapshot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let origin = dir.join("origin");
        let snap = dir.join("snap");
        std::fs::create_dir_all(&origin).unwrap();
        std::fs::write(origin.join("README.md"), "hello\n").unwrap();

        let git = |args: &[&str]| {
            let status = Command::new("git")
                .current_dir(&origin)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {:?} failed", args);
        };
        git(&["init", "-q"]);
        git(&["add", "README.md"]);
        let commit = Command::new("git")
            .current_dir(&origin)
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "init",
            ])
            .status()
            .unwrap();
        assert!(commit.success());
        let oid_str = String::from_utf8(
            Command::new("git")
                .current_dir(&origin)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let oid_str = oid_str.trim();
        let oid = Oid::from_str(oid_str).unwrap();

        let repo = super::git::init_snapshot(&snap, &origin, oid)
            .await
            .unwrap();

        // worktree に commit 内容が checkout されている
        let content = tokio::fs::read_to_string(snap.join("README.md"))
            .await
            .unwrap();
        assert_eq!(content, "hello\n");
        // HEAD は detached で oid に一致
        let head = String::from_utf8(
            Command::new("git")
                .current_dir(&snap)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert_eq!(head.trim(), oid_str);
        // ls_files が tracked file を返す
        let files = repo.ls_files().await.unwrap();
        assert!(files.iter().any(|p| p == std::path::Path::new("README.md")));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

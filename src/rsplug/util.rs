use std::path::PathBuf;

use super::error::Error;

/// 外部コマンドを実行する
macro_rules! execute {
    ($cmd:expr, $($arg:expr),*) => {
        execute![cwd: ".", $cmd, $($arg),*]
    };
    (cwd: $cwd:expr, $cmd:expr, $($arg:expr),*) => {{
            let mut cmd = tokio::process::Command::new($cmd);
                cmd
                .current_dir($cwd)
                $(
                    .arg($arg)
                )*;
            async move {
                let std::process::Output {
                    stdout,
                    status,
                    stderr,
                } = cmd.output().await?;
                if status.success() {
                    Ok(stdout)
                } else {
                    Err(Error::ProcessFailed { stderr })
                }
            }
    }}
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
    //! 各種 Git 操作を行うモジュール
    use std::{
        path::{Path, PathBuf},
        str::FromStr,
        sync::{Arc, Mutex, MutexGuard},
    };

    use git2::{DiffFormat, DiffOptions, Repository, build::CheckoutBuilder};
    use once_cell::sync::Lazy;
    use regex::Regex;
    use tokio::task::spawn_blocking;
    use xxhash_rust::xxh3::Xxh3;

    use super::*;

    #[derive(Clone)]
    pub struct Repo(Arc<Mutex<Repository>>);

    impl From<Repository> for Repo {
        fn from(value: Repository) -> Self {
            Repo(Arc::new(Mutex::new(value)))
        }
    }

    impl Repo {
        #[inline]
        fn borrow<'a>(&'a self) -> MutexGuard<'a, git2::Repository> {
            self.0.lock().unwrap()
        }
    }

    pub async fn ls_files(repo: Repo) -> Result<impl Iterator<Item = PathBuf>, Error> {
        Ok(repo
            .borrow()
            .index()?
            .iter()
            .map(|entry| bytes_to_pathbuf(entry.path))
            .collect::<Vec<_>>()
            .into_iter())
    }

    /// リポジトリが存在するかどうか
    pub async fn open(dir: &Path) -> Result<Repo, Error> {
        Ok(git2::Repository::open(dir)?.into())
    }

    /// リポジトリ初期化処理
    pub async fn init(
        repo: impl AsRef<str> + Send + 'static,
        dir: impl AsRef<Path> + Send + 'static,
    ) -> Result<Repo, Error> {
        let _ = tokio::fs::remove_dir_all(dir.as_ref().join(".git")).await;
        let r = spawn_blocking(move || git2::Repository::init(dir))
            .await
            .unwrap()?;
        spawn_blocking(move || {
            r.remote("origin", repo.as_ref())?;
            Ok(r.into())
        })
        .await
        .unwrap()
    }

    #[derive(Eq, PartialEq, PartialOrd, Ord)]
    enum GitRefType<'a> {
        Other(&'a str),
        Tag(&'a str),
        Pull(usize, &'a str),
        SemVer {
            major: usize,
            minor: usize,
            patch: usize,
        },
        Head,
    }

    impl<'a> From<&'a str> for GitRefType<'a> {
        fn from(value: &'a str) -> Self {
            static PULL_REGEX: Lazy<Regex> = Lazy::new(|| {
                Regex::new(r"^refs/pull/(?<num>[0-9]+)/(?<type>head|merge)$").unwrap()
            });
            static SEMVER_REGEX: Lazy<Regex> = Lazy::new(|| {
                Regex::new(r"^refs/tags/v?(?<major>[0-9]+)\.(?<minor>[0-9]+)\.(?<patch>[0-9]+)$")
                    .unwrap()
            });
            if value == "HEAD" {
                return GitRefType::Head;
            }
            if let Some(caps) = PULL_REGEX.captures(value) {
                let num = usize::from_str(caps.name("num").unwrap().as_str()).unwrap();
                let r#type = caps.name("type").unwrap().as_str();
                return GitRefType::Pull(num, r#type);
            }
            if let Some(caps) = SEMVER_REGEX.captures(value) {
                let major = usize::from_str(caps.name("major").unwrap().as_str()).unwrap();
                let minor = usize::from_str(caps.name("minor").unwrap().as_str()).unwrap();
                let patch = usize::from_str(caps.name("patch").unwrap().as_str()).unwrap();
                return GitRefType::SemVer {
                    major,
                    minor,
                    patch,
                };
            }
            if let Some(inner) = value.strip_prefix("refs/tags/") {
                return GitRefType::Tag(inner);
            }
            GitRefType::Other(value)
        }
    }

    impl<'a> TryFrom<&'a str> for GitRef<'a> {
        type Error = &'static str;
        fn try_from(value: &'a str) -> Result<Self, Self::Error> {
            static LINE_REGEX: Lazy<Regex> =
                Lazy::new(|| Regex::new(r"^(?<id>[0-9a-f]+)\s+(?<gitref>.+)$").unwrap());
            let Some(caps) = LINE_REGEX.captures(value) else {
                return Err("Invalid git ref format");
            };
            let Some(id) = caps.name("id") else {
                return Err("Invalid git ref format: missing id");
            };
            let Some(gitref) = caps.name("gitref") else {
                return Err("Invalid git ref format: missing content");
            };
            Ok(GitRef {
                id: id.as_str(),
                ref_type: GitRefType::from(gitref.as_str()),
            })
        }
    }

    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    struct GitRef<'a> {
        ref_type: GitRefType<'a>,
        id: &'a str,
    }

    /// リポジトリのリモートからrevに対応する最新のコミットハッシュを取得する
    pub async fn ls_remote(url: Arc<str>, rev: &Option<String>) -> Result<String, Error> {
        let rev = rev.as_deref().unwrap_or("HEAD");
        let stdout = execute!["git", "ls-remote", url.as_ref(), rev].await?;
        let Some(latest) = String::from_utf8(stdout)?
            .lines()
            .filter_map(|l| GitRef::try_from(l).ok())
            .max()
            .map(|git_ref| git_ref.id.to_string())
        else {
            return Err(Error::GitRev {
                url,
                rev: rev.to_owned(),
            });
        };

        Ok(latest)
    }

    /// リポジトリ同期処理
    pub async fn fetch(rev: Option<String>, repo: Repo) -> Result<(), Error> {
        execute![
            cwd: repo.0.lock().unwrap().workdir().unwrap(),
            "git",
            "fetch",
            "--depth=1",
            "origin",
            rev.as_deref().unwrap_or("HEAD")
        ]
        .await?;

        fn inner(repo: &Repository) -> Result<(), Error> {
            // TODO: こちらに移行したいが、現状では下記コードでは正常に FETCH_HEAD を取得してくれない
            // repo.find_remote("origin").unwrap().fetch(
            //     &[rev.as_ref().map_or("HEAD", |v| v)],
            //     Some(
            //         FetchOptions::new()
            //             .download_tags(git2::AutotagOption::All)
            //             .depth(1),
            //     ),
            //     None,
            // )?;

            let fetch_head = repo
                .find_reference("FETCH_HEAD")?
                .target()
                .ok_or_else(|| git2::Error::from_str("FETCH_HEAD has no target"))?;

            repo.set_head_detached(fetch_head)?;
            let obj = repo.find_object(fetch_head, None)?;
            repo.checkout_tree(
                &obj,
                Some(
                    CheckoutBuilder::new()
                        .force()
                        .remove_untracked(true)
                        .use_theirs(true)
                        .allow_conflicts(true),
                ),
            )?;

            Ok(())
        }

        spawn_blocking(move || inner(&repo.borrow())).await.unwrap()
    }

    /// HEAD のハッシュ
    pub async fn head_hash(repo: Repo) -> Result<Vec<u8>, Error> {
        spawn_blocking(move || {
            let oid = repo
                .borrow()
                .head()?
                .target()
                .ok_or_else(|| git2::Error::from_str("HEAD is not a direct reference"))?;
            Ok(oid.to_string().into_bytes())
        })
        .await
        .unwrap()
    }

    /// diff の出力
    pub async fn diff_hash(repo: Repo) -> Result<[u8; 16], Error> {
        fn inner(repo: &Repository) -> Result<[u8; 16], Error> {
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
        }
        spawn_blocking(move || inner(&repo.borrow())).await.unwrap()
    }
}

pub mod github {
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
}

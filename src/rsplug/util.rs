use std::process::Output;

use tokio::process::Command;

use super::error::Error;

type ExecuteResult<T = Vec<u8>> = Result<T, Error>;

/// 外部コマンドを実行する
pub async fn execute(cmd: &mut Command) -> ExecuteResult {
    let Output {
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

pub mod git {
    //! 各種 Git 操作を行うモジュール
    use std::{path::Path, str::FromStr, sync::Arc};

    use git2::{DiffFormat, DiffOptions, Repository, build::CheckoutBuilder};
    use once_cell::sync::Lazy;
    use regex::Regex;
    use tokio::task::spawn_blocking;
    use xxhash_rust::xxh3::Xxh3;

    use super::*;

    /// リポジトリが存在するかどうか
    pub async fn exists(dir: &Path) -> bool {
        matches!(
            tokio::fs::try_exists(dir.join(".git").join("HEAD")).await,
            Ok(true)
        )
    }

    /// リポジトリ初期化処理
    pub async fn init(
        repo: impl AsRef<str> + Send + 'static,
        dir: impl AsRef<Path> + Send + 'static,
    ) -> ExecuteResult<Repository> {
        let _ = tokio::fs::remove_dir_all(dir.as_ref().join(".git")).await;
        let r = spawn_blocking(move || git2::Repository::init(dir))
            .await
            .unwrap()?;
        spawn_blocking(move || {
            r.remote("origin", repo.as_ref())?;
            Ok(r)
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
    pub async fn ls_remote(url: Arc<str>, rev: &Option<String>) -> ExecuteResult<String> {
        let rev = rev.as_deref().unwrap_or("HEAD");
        let stdout = execute(
            Command::new("git")
                .arg("ls-remote")
                .arg(url.as_ref())
                .arg(rev),
        )
        .await?;
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
    pub async fn fetch(
        rev: Option<String>,
        dir: impl AsRef<Path> + Send + 'static,
    ) -> ExecuteResult<()> {
        execute(
            Command::new("git")
                .current_dir(&dir)
                .arg("fetch")
                .arg("--depth=1")
                .arg("origin")
                .arg(rev.as_deref().unwrap_or("HEAD")),
        )
        .await?;

        spawn_blocking(move || {
            let repo = git2::Repository::open(dir)?;

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
        })
        .await
        .unwrap()
    }

    /// HEAD のハッシュ
    pub async fn head(dir: impl AsRef<Path> + Send + 'static) -> ExecuteResult {
        spawn_blocking(|| {
            let repo = Repository::open(dir)?;
            let oid = repo
                .head()?
                .target()
                .ok_or_else(|| git2::Error::from_str("HEAD is not a direct reference"))?;
            Ok(oid.to_string().into_bytes())
        })
        .await
        .unwrap()
    }

    /// diff の出力
    pub async fn diff_hash(dir: &Path) -> ExecuteResult<[u8; 16]> {
        let repo = Repository::open(dir)?;

        // HEAD ツリー
        let head_commit = repo.head()?.peel_to_commit()?;
        let head_tree = head_commit.tree()?;

        // diff（git diff HEAD 相当）
        let mut diff_opts = DiffOptions::new();
        // 未追跡も含めたいなら: diff_opts.include_untracked(true);
        let diff = repo.diff_tree_to_workdir(Some(&head_tree), Some(&mut diff_opts))?;

        // パッチ出力を逐次ハッシュ化
        let mut hasher = Xxh3::new();
        diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
            hasher.update(line.content());
            true
        })?;

        // 128bit のダイジェストを hex で
        let digest = hasher.digest128();
        Ok(digest.to_ne_bytes())
    }
}

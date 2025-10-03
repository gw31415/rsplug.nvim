use std::path::PathBuf;

use super::error::Error;

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
        path::{Path, PathBuf},
        str::FromStr,
        sync::{Arc, Mutex},
    };

    use git2::{DiffFormat, DiffOptions, FetchOptions, Oid, build::CheckoutBuilder};
    use once_cell::sync::Lazy;
    use regex::Regex;
    use tokio::task::spawn_blocking;
    use xxhash_rust::xxh3::Xxh3;

    use super::*;

    /// 初期化済みのローカルリポジトリ
    pub struct Repository(Arc<Mutex<git2::Repository>>);

    impl Repository {
        /// (INTERNAL) git2のRepositoryから生成
        fn from(value: git2::Repository) -> Self {
            Repository(Arc::new(Mutex::new(value)))
        }

        /// リポジトリ内のファイル一覧を取得
        pub async fn ls_files(&self) -> Result<impl Iterator<Item = PathBuf>, Error> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .index()?
                .iter()
                .map(|entry| bytes_to_pathbuf(entry.path))
                .collect::<Vec<_>>()
                .into_iter())
        }

        /// リポジトリ同期処理
        pub async fn fetch(&mut self, rev: Oid) -> Result<(), Error> {
            let repo = self.0.clone();
            spawn_blocking(move || {
                let repo = repo.lock().unwrap();
                let obj = if let Ok(obj) = repo.find_object(rev, None) {
                    obj
                } else {
                    if let Ok(mut remote) = repo.find_remote("origin") {
                        remote.fetch(
                            &[rev.to_string()],
                            Some(
                                FetchOptions::new()
                                    .download_tags(git2::AutotagOption::None)
                                    .depth(1),
                            ),
                            None,
                        )?;
                    }
                    repo.find_object(rev, None)?
                };

                repo.set_head_detached(rev)?;
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
        pub async fn head_hash(&self) -> Result<Vec<u8>, Error> {
            let repo = self.0.clone();
            spawn_blocking(move || {
                let oid = repo
                    .lock()
                    .unwrap()
                    .head()?
                    .target()
                    .ok_or_else(|| git2::Error::from_str("HEAD is not a direct reference"))?;
                Ok(oid.to_string().into_bytes())
            })
            .await
            .unwrap()
        }

        /// diff のハッシュの出力
        pub async fn diff_hash(&self) -> Result<[u8; 16], Error> {
            let repo = self.0.clone();
            spawn_blocking(move || {
                let repo = repo.lock().unwrap();
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
    }

    /// リポジトリを開く
    pub async fn open(dir: impl AsRef<Path>) -> Result<Repository, Error> {
        Ok(Repository::from(git2::Repository::open(dir)?))
    }

    /// リポジトリ初期化処理
    pub async fn init(
        dir: impl AsRef<Path> + Send + 'static,
        repo: impl AsRef<str> + Send + 'static,
    ) -> Result<Repository, Error> {
        let _ = tokio::fs::remove_dir_all(dir.as_ref().join(".git")).await;
        let r = spawn_blocking(move || git2::Repository::init(dir))
            .await
            .unwrap()?;
        spawn_blocking(move || {
            r.remote("origin", repo.as_ref())?;
            Ok(Repository::from(r))
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
    pub async fn ls_remote(url: Arc<str>, rev: &Option<String>) -> Result<Oid, Error> {
        let mut remote = git2::Remote::create_detached(url.to_string()).unwrap();
        let connection = remote
            .connect_auth(git2::Direction::Fetch, None, None)
            .unwrap();
        let references = connection.list().unwrap();
        let latest = if let Some(rev) = rev {
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
    }
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
}

pub mod glob {
    use std::{borrow::Cow, path::Path};

    use hashbrown::HashMap;
    use ignore::{WalkBuilder, overrides::OverrideBuilder};

    pub fn find<'a>(
        pattern: impl IntoIterator<Item = &'a str>,
    ) -> Result<impl Iterator<Item = Result<Cow<'a, Path>, ignore::Error>>, ignore::Error> {
        let mut hashmap: HashMap<&Path, (WalkBuilder, OverrideBuilder)> = HashMap::new();
        let mut raw_path = Vec::new();
        for pattern in pattern {
            let ParsedGlob { base, pattern } = ParsedGlob::new(pattern);
            if pattern.is_empty() {
                raw_path.push(Ok(base.into()));
            } else {
                hashmap
                    .entry(base)
                    .or_insert_with(|| {
                        let mut builder = WalkBuilder::new(base);
                        builder
                            .standard_filters(false)
                            .skip_stdout(true)
                            .hidden(false)
                            .max_depth(Some(128))
                            .follow_links(true);
                        (builder, OverrideBuilder::new(base))
                    })
                    .1
                    .add(pattern)?;
            }
        }

        let iter = hashmap
            .into_values()
            .map(|(mut builder, overrides)| {
                Ok::<_, ignore::Error>(builder.overrides(overrides.build()?).build())
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .map(|entry| entry.map(|entry| entry.into_path().into()))
            .chain(raw_path);

        Ok(iter)
    }

    struct ParsedGlob<'a> {
        base: &'a Path,
        pattern: &'a str,
    }
    impl<'a> ParsedGlob<'a> {
        fn new(pattern: &'a str) -> ParsedGlob<'a> {
            let pattern_path = Path::new(pattern);

            macro_rules! anyof { ($c1: literal$(, $c: literal)*) => { |ch: char| { ch == $c1 $( || ch == $c )* } }; }

            let special_comp = pattern_path
                .components()
                .map(|comp| comp.as_os_str().to_str().unwrap())
                .find(|comp| comp.contains(anyof!['*', '?', '[', ']']));
            if let Some(special_comp) = special_comp {
                let pos = pattern.find(special_comp).unwrap();
                let (path, pattern) = pattern.split_at(pos);
                ParsedGlob {
                    base: Path::new(path),
                    pattern,
                }
            } else {
                ParsedGlob {
                    base: pattern_path,
                    pattern: "",
                }
            }
        }
    }
}

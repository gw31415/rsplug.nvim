use std::{path::PathBuf, str::FromStr, sync::Arc};

use dag::{DagError, TryDag, iterator::DagIteratorMapFuncArgs};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_with::DeserializeFromStr;

use super::*;

/// 設定を構成する基本単位
pub struct Unit {
    /// 取得元
    pub source: PluginSource,
    /// Unitに対応する読み込みタイプ
    pub lazy_type: LazyType,
    /// セットアップスクリプト
    pub script: SetupScript,
    /// マージ設定
    pub merge: MergeConfig,
}

/// プラグインの取得元
#[derive(DeserializeFromStr)]
pub enum UnitSource {
    /// GitHub リポジトリ
    GitHub {
        /// リポジトリの所有者
        owner: String,
        /// リポジトリ
        repo: Arc<str>,
        /// リビジョン
        rev: Option<String>,
    },
}

impl UnitSource {
    pub fn url(&self) -> String {
        match self {
            UnitSource::GitHub { owner, repo, .. } => util::github::url(owner, repo),
        }
    }
    pub fn cachedir(&self) -> PathBuf {
        // Such as [Given: ~/.cache/rsplug/]./github.com/owner/repo
        match self {
            UnitSource::GitHub { owner, repo, .. } => {
                let mut path = PathBuf::new();
                path.push("repos");
                path.push("github.com");
                path.push(owner);
                path.push(repo.as_ref());
                path
            }
        }
    }
}

impl FromStr for UnitSource {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        static GITHUB_REPO_REGEX: Lazy<Regex> = Lazy::new(|| {
            Regex::new(r"^(?<owner>[a-zA-Z0-9]([a-zA-Z0-9]?|[\-]?([a-zA-Z0-9])){0,38})/(?<repo>[a-zA-Z0-9][a-zA-Z0-9_.-]{0,38})(@(?<rev>\S+))?$").unwrap()
        });
        let Some(cap) = GITHUB_REPO_REGEX.captures(s) else {
            return Err("GitHub repository format must be 'owner/repo[@rev]'");
        };
        let owner = cap["owner"].to_string();
        let repo = cap["repo"].into();
        let rev = cap.name("rev").map(|rev| rev.as_str().to_string());
        Ok(UnitSource::GitHub { owner, repo, rev })
    }
}

#[derive(thiserror::Error, Debug)]
#[error("DAG creation error")]
pub struct DAGCreationError(#[from] DagError);

impl Unit {
    /// 設定ファイルから Unit のコレクションを構築する
    pub fn new(config: Config) -> Result<impl Iterator<Item = Unit>, DAGCreationError> {
        let Config { plugins } = config;
        Ok(plugins.try_dag()?.into_map_iter(
            |DagIteratorMapFuncArgs {
                 inner,
                 dependents_iter,
             }| {
                // 依存元の lazy_type を集約
                let lazy_type = dependents_iter
                    .flatten()
                    .fold(inner.lazy_type(), |dep, plug| dep & plug.lazy_type());
                let Plugin {
                    repo,
                    start: _,
                    on_event: _,
                    on_cmd: _,
                    on_ft: _,
                    on_map: _,
                    depends: _,
                    custom_name: _,
                    script,
                    merge,
                } = inner;
                Unit {
                    source: repo,
                    lazy_type,
                    script: script.into(),
                    merge,
                }
            },
        ))
    }
}

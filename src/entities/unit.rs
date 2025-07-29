use std::sync::Arc;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::entities::config::Plugin;

use super::*;

/// 設定を構成する基本単位
pub struct Unit {
    /// 取得元
    pub source: UnitSource,
    /// Unitに対応する読み込みタイプ
    pub lazy_type: LazyType,
    /// 依存する Unit のリスト
    pub depends: Vec<Arc<Unit>>,
    /// セットアップスクリプト
    pub script: SetupScript,
}

/// プラグインの取得元
pub enum UnitSource {
    /// GitHub リポジトリ
    GitHub {
        /// リポジトリの所有者
        owner: String,
        /// リポジトリ
        repo: String,
        /// リビジョン
        rev: Option<String>,
    },
}

impl Unit {
    /// 設定ファイルから Unit のコレクションを構築する
    pub fn new(config: Config) -> MainResult<Vec<Arc<Unit>>> {
        static GITHUB_REPO_REGEX: Lazy<Regex> = Lazy::new(|| {
            Regex::new(r"^(?<owner>[a-zA-Z0-9]([a-zA-Z0-9]?|[\-]?([a-zA-Z0-9])){0,38})/(?<repo>[a-zA-Z0-9][a-zA-Z0-9_.-]{0,38})$").unwrap()
        });
        let Config { plugins } = config;
        let mut units: Vec<Arc<Unit>> = Vec::new();
        for plugin in plugins {
            let Plugin {
                start,
                repo,
                on_event,
                rev,
                script,
            } = plugin;
            let lazy_type = if start {
                LazyType::Start
            } else {
                LazyType::Opt(on_event.into_iter().map(LoadEvent::Autocmd).collect())
            };
            let source = {
                if let Some(captures) = GITHUB_REPO_REGEX.captures(&repo) {
                    let repo_start = captures.name("repo").unwrap().start();
                    let mut owner = repo;
                    let repo = owner.split_off(repo_start);
                    owner.pop();
                    UnitSource::GitHub { owner, repo, rev }
                } else {
                    return Err(Error::Serde(serde::de::Error::custom(format!(
                        "Invalid repo format: {repo}",
                    ))));
                }
            };
            let unit = Arc::new(Unit {
                source,
                lazy_type,
                depends: Vec::new(),
                script,
            });
            units.push(unit);
        }
        Ok(units)
    }
}

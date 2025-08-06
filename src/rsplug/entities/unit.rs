use std::{str::FromStr, sync::Arc};

use once_cell::sync::Lazy;
use regex::Regex;
use serde_with::DeserializeFromStr;

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
        repo: String,
        /// リビジョン
        rev: Option<String>,
    },
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
        let repo = cap["repo"].to_string();
        let rev = cap.name("rev").map(|rev| rev.as_str().to_string());
        Ok(UnitSource::GitHub { owner, repo, rev })
    }
}

impl Unit {
    /// 設定ファイルから Unit のコレクションを構築する
    pub fn new(config: Config) -> Vec<Arc<Unit>> {
        let Config { plugins } = config;
        let mut units: Vec<Arc<Unit>> = Vec::new();
        for plugin in plugins {
            let Plugin {
                start,
                repo: source,
                on_event,
                on_cmd,
                on_ft,
                script,
                merge,
            } = plugin;
            let lazy_type = if start {
                LazyType::Start
            } else {
                LazyType::Opt(
                    on_event
                        .into_iter()
                        .map(LoadEvent::Autocmd)
                        .chain(on_cmd.into_iter().map(LoadEvent::UserCmd))
                        .chain(on_ft.into_iter().map(LoadEvent::FileType))
                        .collect(),
                )
            };
            let unit = Arc::new(Unit {
                source,
                lazy_type,
                depends: Vec::new(),
                script,
                merge,
            });
            units.push(unit);
        }
        units
    }
}

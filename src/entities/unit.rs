use std::sync::Arc;

use config_file::PluginConfig;
use once_cell::sync::Lazy;
use regex::Regex;

use super::*;

/// 設定を構成する基本単位
pub struct Unit {
    /// 取得元
    pub source: UnitSource,
    /// Unitに対応する読み込みタイプ
    pub lazy_type: LazyType,
    /// 依存する Unit のリスト
    pub depends: Vec<Arc<Unit>>,
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
    pub fn new(config: impl Into<PluginConfig>) -> MainResult<Vec<Arc<Unit>>> {
        static GITHUB_REPO_REGEX: Lazy<Regex> = Lazy::new(|| {
            Regex::new(r"^(?<owner>[a-zA-Z0-9]([a-zA-Z0-9]?|[\-]?([a-zA-Z0-9])){0,38})/(?<repo>[a-zA-Z0-9][a-zA-Z0-9_.-]{0,38})$").unwrap()
        });
        let PluginConfig { plugins } = config.into();
        let mut units: Vec<Arc<Unit>> = Vec::new();
        for plugin in plugins {
            let lazy_type = if plugin.start {
                LazyType::Start
            } else {
                LazyType::Opt(
                    plugin
                        .on_event
                        .into_iter()
                        .map(LoadEvent::Autocmd)
                        .collect(),
                )
            };
            let source = {
                if let Some(captures) = GITHUB_REPO_REGEX.captures(&plugin.repo) {
                    let (owner, repo) = (&captures["owner"], &captures["repo"]);
                    UnitSource::GitHub {
                        owner: owner.to_string(),
                        repo: repo.to_string(),
                        rev: plugin.rev,
                    }
                } else {
                    return Err(Error::Serde(serde::de::Error::custom(format!(
                        "Invalid repo format: {}",
                        plugin.repo
                    ))));
                }
            };
            let unit = Arc::new(Unit {
                source,
                lazy_type,
                depends: Vec::new(),
            });
            units.push(unit);
        }
        Ok(units)
    }
}

mod config_file {
    use std::{iter::Sum, ops::AddAssign};

    use serde::Deserialize;
    use serde_with::{OneOrMany, serde_as};

    impl<T: IntoIterator<Item = PluginConfig>> From<T> for PluginConfig {
        fn from(value: T) -> Self {
            value.into_iter().sum()
        }
    }

    /// 設定ファイルの構造体
    #[serde_as]
    #[derive(Deserialize)]
    pub struct PluginConfig {
        pub(super) plugins: Vec<Plugin>,
    }

    impl AddAssign for PluginConfig {
        fn add_assign(&mut self, rhs: Self) {
            self.plugins.extend(rhs.plugins);
        }
    }

    impl Sum for PluginConfig {
        fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
            let mut res = PluginConfig {
                plugins: Default::default(),
            };
            for plugin in iter {
                res += plugin;
            }
            res
        }
    }

    #[serde_as]
    #[derive(Deserialize)]
    pub(super) struct Plugin {
        pub repo: String,
        #[serde(default)]
        pub start: bool,
        #[serde_as(as = "OneOrMany<_>")]
        #[serde(default)]
        pub on_event: Vec<String>,
        pub rev: Option<String>,
    }
}

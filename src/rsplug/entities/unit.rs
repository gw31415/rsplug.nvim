use std::{cell::RefCell, collections::BTreeSet, str::FromStr, sync::Arc};

use hashbrown::{HashMap, HashSet};
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

struct FilteredIterator<'a, T, F: Fn(&T) -> bool> {
    index: usize,
    vec: &'a mut Vec<T>,
    filter: F,
}

impl<T, F: Fn(&T) -> bool> FilteredIterator<'_, T, F> {
    fn new<'a>(vec: &'a mut Vec<T>, filter: F) -> FilteredIterator<'a, T, F> {
        FilteredIterator {
            index: vec.len(),
            vec,
            filter,
        }
    }
}

impl<T, F: Fn(&T) -> bool> Iterator for FilteredIterator<'_, T, F> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> {
        let Self { index, vec, filter } = self;
        loop {
            if *index == 0 {
                return None;
            }
            *index -= 1;
            let item = (*vec).get(*index).unwrap();
            if filter(item) {
                return Some(vec.swap_remove(*index));
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
        let repo = cap["repo"].to_string();
        let rev = cap.name("rev").map(|rev| rev.as_str().to_string());
        Ok(UnitSource::GitHub { owner, repo, rev })
    }
}

#[derive(thiserror::Error, Debug)]
#[error("DAG creation error")]
pub struct DAGCreationError;

impl Unit {
    /// 設定ファイルから Unit のコレクションを構築する
    pub fn new(config: Config) -> Result<impl Iterator<Item = Arc<Unit>>, DAGCreationError> {
        let Config { mut plugins } = config;
        let units = RefCell::new(HashMap::<String, Vec<Arc<Unit>>>::new());
        loop {
            let size_before = plugins.len();
            let iterator = FilteredIterator::new(&mut plugins, |item| {
                let units = units.borrow();
                item.depends.iter().all(|dep| units.contains_key(dep))
            });
            for plugin in iterator {
                let name = plugin.name().to_string();
                let Plugin {
                    start,
                    repo: source,
                    on_event,
                    on_cmd,
                    on_ft,
                    script,
                    merge,
                    depends,
                    custom_name: _,
                    on_map,
                } = plugin;
                let lazy_type = if start {
                    LazyType::Start
                } else {
                    LazyType::Opt({
                        let mut set: BTreeSet<_> = on_event
                            .into_iter()
                            .map(LoadEvent::Autocmd)
                            .chain(on_cmd.into_iter().map(LoadEvent::UserCmd))
                            .chain(on_ft.into_iter().map(LoadEvent::FileType))
                            .collect();
                        set.insert(LoadEvent::OnMap(on_map));
                        set
                    })
                };
                let unit = {
                    let units = units.borrow();
                    Arc::new(Unit {
                        source,
                        lazy_type,
                        depends: depends
                            .iter()
                            .collect::<HashSet<_>>() // 重複削除
                            .into_iter()
                            .flat_map(|dep| units.get(dep).unwrap().clone())
                            .collect(),
                        script,
                        merge,
                    })
                };
                units.borrow_mut().entry(name).or_default().push(unit);
            }
            if size_before == plugins.len() {
                if plugins.is_empty() {
                    break;
                }

                return Err(DAGCreationError);
            }
        }
        Ok(units.into_inner().into_values().flatten())
    }
}

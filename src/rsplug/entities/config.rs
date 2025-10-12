use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    hash::Hash,
    iter::Sum,
    ops::AddAssign,
    path::Path,
    str::FromStr,
    sync::Arc,
};

use dag::DagNode;
use hashbrown::HashMap;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use sailfish::runtime::Render;
use serde::Deserialize;
use serde_with::{DeserializeFromStr, OneOrMany, serde_as};

use super::*;

impl<T: IntoIterator<Item = Config>> From<T> for Config {
    fn from(value: T) -> Self {
        value.into_iter().sum()
    }
}

/// 設定ファイルの構造体
#[serde_as]
#[derive(Deserialize)]
pub struct Config {
    #[serde(default)]
    pub(super) plugins: Vec<Plugin>,
}

impl AddAssign for Config {
    fn add_assign(&mut self, rhs: Self) {
        self.plugins.extend(rhs.plugins);
    }
}

impl Sum for Config {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        let mut res = Config {
            plugins: Default::default(),
        };
        for plugin in iter {
            res += plugin;
        }
        res
    }
}

#[derive(Deserialize)]
pub struct PluginSource {
    #[serde(rename = "repo")]
    pub base: UnitSource,
    #[serde(default, rename = "sym")]
    pub manually_to_sym: bool,
    #[serde(default)]
    pub build: Vec<String>,
}

impl PluginSource {
    pub fn to_sym(&self) -> bool {
        self.manually_to_sym || !self.build.is_empty()
    }
}

#[serde_as]
#[derive(Deserialize)]
pub(super) struct Plugin {
    #[serde(flatten)]
    pub repo: PluginSource,
    #[serde(default)]
    pub start: bool,
    #[serde_as(as = "OneOrMany<_>")]
    #[serde(default)]
    pub on_event: Vec<Autocmd>,
    #[serde_as(as = "OneOrMany<_>")]
    #[serde(default)]
    pub on_cmd: Vec<UserCmd>,
    #[serde_as(as = "OneOrMany<_>")]
    #[serde(default)]
    pub on_ft: Vec<FileType>,
    #[serde(default)]
    pub on_map: KeyPattern,
    #[serde_as(as = "OneOrMany<_>")]
    #[serde(default)]
    pub depends: Vec<String>,
    #[serde(rename = "name")]
    pub custom_name: Option<String>,
    #[serde(flatten)]
    pub script: SetupScriptOne,
    #[serde(flatten)]
    pub merge: MergeConfig,
}

impl DagNode for Plugin {
    fn id(&self) -> &str {
        self.custom_name.as_ref().map_or(
            match &self.repo.base {
                UnitSource::GitHub { repo, .. } => repo.as_ref(),
            },
            |v| v,
        )
    }
    fn depends(&self) -> impl IntoIterator<Item = &impl AsRef<str>> {
        &self.depends
    }
}

impl Plugin {
    /// プラグインに設定されたLazyTypeを生成する
    pub(super) fn lazy_type(&self) -> LazyType {
        let Self {
            repo: _,
            start,
            on_event,
            on_cmd,
            on_ft,
            on_map,
            depends: _,
            custom_name: _,
            script: _,
            merge: _,
        } = self;

        if *start {
            LazyType::Start
        } else {
            LazyType::Opt({
                let mut set: BTreeSet<_> = on_event
                    .iter()
                    .map(|a| LoadEvent::Autocmd(a.clone()))
                    .chain(on_cmd.iter().map(|a| LoadEvent::UserCmd(a.clone())))
                    .chain(on_ft.iter().map(|a| LoadEvent::FileType(a.clone())))
                    .collect();
                set.insert(LoadEvent::OnMap(on_map.clone()));
                set
            })
        }
    }
}

/// プラグインのセットアップに用いるスクリプト群
#[derive(Deserialize, Default)]
pub(crate) struct SetupScriptOne {
    /// プラグイン読み込み直後に実行される Lua スクリプト
    pub lua_after: Option<String>,
    /// プラグイン読み込み直前に実行される Lua スクリプト
    pub lua_before: Option<String>,
}

/// プラグインのセットアップに用いるスクリプト群
#[derive(Clone, Default)]
pub struct SetupScript {
    /// プラグイン読み込み直後に実行される Lua スクリプト
    pub lua_after: BTreeSet<String>,
    /// プラグイン読み込み直前に実行される Lua スクリプト
    pub lua_before: BTreeSet<String>,
}

impl From<SetupScriptOne> for SetupScript {
    fn from(value: SetupScriptOne) -> Self {
        let SetupScriptOne {
            lua_after,
            lua_before,
        } = value;
        SetupScript {
            lua_after: lua_after.into_iter().collect(),
            lua_before: lua_before.into_iter().collect(),
        }
    }
}

impl AddAssign for SetupScript {
    fn add_assign(&mut self, rhs: Self) {
        self.lua_after.extend(rhs.lua_after);
        self.lua_before.extend(rhs.lua_before);
    }
}

/// プラグインのセットアップに用いるスクリプト群
#[derive(Deserialize)]
pub struct MergeConfig {
    #[serde(default = "default_ignore")]
    pub ignore: FileSpecifier,
}

fn default_ignore() -> FileSpecifier {
    FileSpecifier::from_str(include_str!("../../../templates/ignore.gitignore")).unwrap()
}

/// Gitignore形式のファイル指定子
#[derive(DeserializeFromStr)]
pub struct FileSpecifier(Arc<Gitignore>);

impl FileSpecifier {
    pub fn matched(&self, filepath: impl AsRef<Path>) -> bool {
        self.0.matched(filepath.as_ref(), false).is_ignore()
    }
}

impl FromStr for FileSpecifier {
    type Err = ignore::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut builder = GitignoreBuilder::new("");
        for line in s.lines() {
            builder.add_line(None, line)?;
        }
        Ok(FileSpecifier(builder.build()?.into()))
    }
}

/// キーパターン
#[derive(Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyPattern(pub BTreeMap<ModeChar, Vec<Arc<String>>>);

#[derive(Hash, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct ModeChar(Option<char>);

impl std::fmt::Display for ModeChar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(c) => c.fmt(f),
            None => "".fmt(f),
        }
    }
}

impl Render for ModeChar {
    fn render(&self, b: &mut sailfish::runtime::Buffer) -> Result<(), sailfish::RenderError> {
        match self.0 {
            Some(c) => c.render(b),
            None => "".render(b),
        }
    }
}

#[serde_as]
#[derive(Deserialize)]
#[serde(untagged)]
enum KeyPatternDeserializerInner {
    ModeMap(#[serde_as(as = "HashMap<_, OneOrMany<_>>")] HashMap<String, Vec<String>>),
    AllPattern(#[serde_as(as = "OneOrMany<_>")] Vec<String>),
}

impl<'de> Deserialize<'de> for KeyPattern {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let inner: KeyPatternDeserializerInner =
            KeyPatternDeserializerInner::deserialize(deserializer)?;
        let mut out: BTreeMap<ModeChar, Vec<Arc<String>>> = BTreeMap::new();

        match inner {
            KeyPatternDeserializerInner::ModeMap(map) => {
                for (k, vals) in map {
                    let vals: Vec<Arc<String>> = vals.into_iter().map(Arc::new).collect();
                    // 例: "abc" -> 'a','b','c' 全てに同じ vals を付与
                    for ch in k.chars() {
                        match out.entry(ModeChar(Some(ch))) {
                            Entry::Vacant(e) => {
                                e.insert(vals.clone());
                            }
                            Entry::Occupied(mut e) => {
                                e.get_mut().extend(vals.iter().cloned());
                            }
                        }
                    }
                }
            }
            KeyPatternDeserializerInner::AllPattern(v) => {
                let v: Vec<Arc<String>> = v.into_iter().map(Arc::new).collect();
                // 例: "hogehoge" / ["hoge","fuga"] -> { None: [...] }
                match out.entry(ModeChar(None)) {
                    Entry::Vacant(e) => {
                        e.insert(v);
                    }
                    Entry::Occupied(mut e) => {
                        e.get_mut().extend(v.iter().cloned());
                    }
                }
            }
        }

        Ok(KeyPattern(out))
    }
}

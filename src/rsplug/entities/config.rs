use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    hash::Hash,
    iter::{Sum, once},
    ops::AddAssign,
    path::Path,
    str::FromStr,
    sync::Arc,
};

use dag::DagNode;
use hashbrown::HashMap;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use sailfish::runtime::Render;
use serde::{Deserialize, Serialize};
use serde_with::{DeserializeFromStr, FromInto, OneOrMany, serde_as};

use super::*;

impl<T: IntoIterator<Item = Config>> From<T> for Config {
    fn from(value: T) -> Self {
        value.into_iter().sum()
    }
}

/// 設定ファイルの構造体
#[serde_as]
#[derive(Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub plugins: Vec<PluginConfig>,
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

#[derive(Clone, Deserialize, Serialize)]
pub struct CacheConfig {
    #[serde(rename = "repo")]
    pub repo: RepoSource,
    #[serde(default, rename = "sym")]
    pub manually_to_sym: bool,
    #[serde(default)]
    pub build: Vec<String>,
}

impl CacheConfig {
    pub fn to_sym(&self) -> bool {
        self.manually_to_sym || !self.build.is_empty()
    }
}

#[serde_as]
#[derive(Deserialize, Serialize)]
struct LazyTypeDeserializer {
    #[serde(default)]
    start: bool,
    #[serde_as(as = "OneOrMany<_>")]
    #[serde(default)]
    on_event: Vec<Autocmd>,
    #[serde_as(as = "OneOrMany<_>")]
    #[serde(default)]
    on_cmd: Vec<UserCmd>,
    #[serde_as(as = "OneOrMany<_>")]
    #[serde(default)]
    on_ft: Vec<FileType>,
    #[serde(default)]
    on_map: KeyPattern,
}

impl From<LazyTypeDeserializer> for LazyType {
    fn from(val: LazyTypeDeserializer) -> Self {
        let LazyTypeDeserializer {
            start,
            on_event,
            on_cmd,
            on_ft,
            on_map,
        } = val;
        if start {
            LazyType::Start
        } else {
            LazyType::Opt(
                on_event
                    .into_iter()
                    .map(LoadEvent::Autocmd)
                    .chain(on_cmd.into_iter().map(LoadEvent::UserCmd))
                    .chain(on_ft.into_iter().map(LoadEvent::FileType))
                    .chain(once(LoadEvent::OnMap(on_map)))
                    .collect(),
            )
        }
    }
}

impl From<LazyType> for LazyTypeDeserializer {
    fn from(val: LazyType) -> Self {
        match val {
            LazyType::Start => LazyTypeDeserializer {
                start: true,
                on_event: Vec::new(),
                on_cmd: Vec::new(),
                on_ft: Vec::new(),
                on_map: KeyPattern::default(),
            },
            LazyType::Opt(events) => {
                let mut on_event = Vec::new();
                let mut on_cmd = Vec::new();
                let mut on_ft = Vec::new();
                let mut on_map = KeyPattern::default();

                for event in events {
                    match event {
                        LoadEvent::Autocmd(a) => on_event.push(a),
                        LoadEvent::UserCmd(u) => on_cmd.push(u),
                        LoadEvent::FileType(f) => on_ft.push(f),
                        LoadEvent::OnMap(m) => on_map = m,
                        LoadEvent::LuaModule(_) => {
                            // LuaModule is auto-detected, not from config
                        }
                    }
                }

                LazyTypeDeserializer {
                    start: false,
                    on_event,
                    on_cmd,
                    on_ft,
                    on_map,
                }
            }
        }
    }
}

#[serde_as]
#[derive(Clone, Deserialize, Serialize)]
pub struct PluginConfig {
    #[serde(flatten)]
    pub cache: CacheConfig,
    #[serde(flatten)]
    #[serde_as(as = "FromInto<LazyTypeDeserializer>")]
    pub lazy_type: LazyType,
    #[serde_as(as = "OneOrMany<_>")]
    #[serde(default)]
    pub with: Vec<String>,
    #[serde(rename = "name")]
    pub custom_name: Option<String>,
    #[serde(flatten)]
    #[serde_as(as = "FromInto<SetupScriptOne>")]
    pub script: SetupScript,
    #[serde(flatten)]
    pub merge: MergeConfig,
}

impl DagNode for PluginConfig {
    fn id(&self) -> &str {
        self.custom_name.as_ref().map_or(
            match &self.cache.repo {
                RepoSource::GitHub { repo, .. } => repo.as_ref(),
            },
            |v| v,
        )
    }
    fn depends(&self) -> impl IntoIterator<Item = &impl AsRef<str>> {
        &self.with
    }
}

/// プラグインのセットアップに用いるスクリプト群
#[derive(Deserialize, Default, Serialize)]
struct SetupScriptOne {
    /// プラグイン読み込み直後に実行される Lua スクリプト
    lua_after: Option<String>,
    /// プラグイン読み込み直前に実行される Lua スクリプト
    lua_before: Option<String>,
}

/// プラグインのセットアップに用いるスクリプト群
#[derive(Clone, Default, Debug)]
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

impl From<SetupScript> for SetupScriptOne {
    fn from(value: SetupScript) -> Self {
        let SetupScript {
            lua_after,
            lua_before,
        } = value;
        // Take the first element from each BTreeSet (if any)
        SetupScriptOne {
            lua_after: lua_after.into_iter().next(),
            lua_before: lua_before.into_iter().next(),
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
#[derive(Clone, Deserialize, Serialize)]
pub struct MergeConfig {
    #[serde(default = "default_ignore")]
    pub ignore: FileSpecifier,
}

fn default_ignore() -> FileSpecifier {
    FileSpecifier::from_str(include_str!("../../../templates/ignore.gitignore")).unwrap()
}

/// Gitignore形式のファイル指定子
#[derive(Clone, DeserializeFromStr)]
pub struct FileSpecifier(Arc<Gitignore>, String);

impl std::fmt::Debug for FileSpecifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("FileSpecifier").field(&self.1).finish()
    }
}

impl FileSpecifier {
    pub fn matched(&self, filepath: impl AsRef<Path>) -> bool {
        self.0.matched(filepath.as_ref(), false).is_ignore()
    }
}

impl Serialize for FileSpecifier {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.1)
    }
}

impl FromStr for FileSpecifier {
    type Err = ignore::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut builder = GitignoreBuilder::new("");
        for line in s.lines() {
            builder.add_line(None, line)?;
        }
        Ok(FileSpecifier(builder.build()?.into(), s.to_string()))
    }
}

/// キーパターン
#[derive(Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct KeyPattern(pub BTreeMap<ModeChar, Vec<Arc<String>>>);

impl Serialize for KeyPattern {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            let v_strings: Vec<&str> = v.iter().map(|s| s.as_str()).collect();
            map.serialize_entry(&k.to_string(), &v_strings)?;
        }
        map.end()
    }
}

#[derive(Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Debug)]
pub struct ModeChar(Option<char>);

impl Serialize for ModeChar {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            Some(c) => serializer.serialize_char(c),
            None => serializer.serialize_str(""),
        }
    }
}

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

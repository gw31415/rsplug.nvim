use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    convert::Infallible,
    hash::Hash,
    iter::{Sum, once},
    ops::AddAssign,
    path::Path,
    str::FromStr,
    sync::Arc,
};

use dag::DagNode;
use hashbrown::HashMap;
use sailfish::runtime::Render;
use serde::Deserialize;
use serde_with::{DeserializeFromStr, FromInto, OneOrMany, serde_as};
use wildmatch::WildMatch;

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
    pub(super) plugins: Vec<PluginConfig>,
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
pub struct CacheConfig {
    #[serde(rename = "repo")]
    pub repo: RepoSource,
    #[serde(default, rename = "sym")]
    pub manually_to_sym: bool,
    #[serde(default)]
    pub build: Vec<String>,
    #[serde(default)]
    pub lua_build: Option<String>,
}

impl CacheConfig {
    pub fn to_sym(&self) -> bool {
        self.manually_to_sym || !self.build.is_empty() || self.lua_build.is_some()
    }
}

#[serde_as]
#[derive(Deserialize)]
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
#[derive(Deserialize)]
pub(super) struct PluginConfig {
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
#[derive(Deserialize, Default)]
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
pub struct FileSpecifier(Vec<FileSpecifierPattern>, String);

#[derive(Debug)]
struct FileSpecifierPattern {
    matcher: WildMatch,
    matcher_for_any_depth: Option<WildMatch>,
    path_only: bool,
    negated: bool,
}

impl std::fmt::Debug for FileSpecifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("FileSpecifier").field(&self.1).finish()
    }
}

impl FileSpecifier {
    pub fn matched(&self, filepath: impl AsRef<Path>) -> bool {
        let path = filepath.as_ref().to_string_lossy();
        let path = path.replace('\\', "/");
        let mut ignored = false;

        for pat in &self.0 {
            let matches = if pat.path_only {
                pat.matcher.matches(&path)
                    || pat
                        .matcher_for_any_depth
                        .as_ref()
                        .is_some_and(|m| m.matches(&path))
            } else {
                path.split('/').any(|seg| pat.matcher.matches(seg))
            };

            if matches {
                ignored = !pat.negated;
            }
        }

        ignored
    }
}

impl FromStr for FileSpecifier {
    type Err = Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut patterns = Vec::new();
        for line in s.lines() {
            let mut line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let negated = line.starts_with('!');
            if negated {
                line = line.strip_prefix('!').unwrap_or(line);
            }
            if let Some(rest) = line.strip_prefix(r"\!") {
                line = rest;
            } else if let Some(rest) = line.strip_prefix(r"\#") {
                line = rest;
            }
            if line.is_empty() {
                continue;
            }

            let anchored_to_root = line.starts_with('/');
            let line = line.trim_start_matches('/');
            let had_slash = line.contains('/');
            let directory_only = line.ends_with('/');
            let line = line.trim_end_matches('/');
            if line.is_empty() {
                continue;
            }

            let path_only = had_slash || directory_only;
            let body = if directory_only {
                format!("{line}/**")
            } else {
                line.to_string()
            };
            let matcher_for_any_depth = if path_only && !anchored_to_root {
                Some(WildMatch::new(&format!("**/{body}")))
            } else {
                None
            };

            patterns.push(FileSpecifierPattern {
                matcher: WildMatch::new(&body),
                matcher_for_any_depth,
                path_only,
                negated,
            });
        }
        Ok(FileSpecifier(patterns, s.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::FileSpecifier;
    use std::{path::Path, str::FromStr};

    #[test]
    fn file_specifier_matches_by_segment() {
        let spec = FileSpecifier::from_str("README.md\nLICENSE*").expect("must parse");

        assert!(spec.matched(Path::new("plugin/README.md")));
        assert!(spec.matched(Path::new("foo/LICENSE.txt")));
        assert!(!spec.matched(Path::new("plugin/main.lua")));
    }

    #[test]
    fn file_specifier_supports_directory_pattern() {
        let spec = FileSpecifier::from_str("tests/").expect("must parse");

        assert!(spec.matched(Path::new("tests/a.lua")));
        assert!(spec.matched(Path::new("foo/tests/b.lua")));
        assert!(!spec.matched(Path::new("test/a.lua")));
    }

    #[test]
    fn file_specifier_supports_negation() {
        let spec = FileSpecifier::from_str("*.md\n!README.md").expect("must parse");

        assert!(spec.matched(Path::new("docs/guide.md")));
        assert!(!spec.matched(Path::new("README.md")));
    }

    #[test]
    fn file_specifier_supports_root_anchored_path() {
        let spec = FileSpecifier::from_str("/doc/*.txt").expect("must parse");

        assert!(spec.matched(Path::new("doc/help.txt")));
        assert!(!spec.matched(Path::new("plugin/doc/help.txt")));
    }
}

/// キーパターン
#[derive(Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct KeyPattern(pub BTreeMap<ModeChar, Vec<Arc<String>>>);

#[derive(Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Debug)]
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

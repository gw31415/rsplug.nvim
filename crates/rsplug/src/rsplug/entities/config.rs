use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    hash::Hash,
    iter::{Sum, once},
    ops::AddAssign,
    sync::Arc,
};

use dag::DagNode;
use file_specifier::FileSpecifier;
use hashbrown::HashMap;
use sailfish::runtime::Render;
use serde::{Deserialize, Deserializer};
use serde_with::{FromInto, OneOrMany, serde_as};

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
    #[serde(default, rename = "repo")]
    pub repo: Option<RepoSource>,
    /// pack に `.git` を複製する（git 利用プラグイン用）。`true` だと TarballFetch を無効化し GitFetch に強制する。
    #[serde(default)]
    pub dotgit: bool,
    #[serde(default)]
    pub build: Vec<String>,
    #[serde(default)]
    pub lua_build: Option<String>,
    #[serde(default)]
    pub lua_post_update: Option<String>,
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
    #[serde_as(as = "OneOrMany<_>")]
    #[serde(default)]
    on_func: Vec<VimFunc>,
    #[serde_as(as = "OneOrMany<_>")]
    #[serde(default)]
    on_source: Vec<String>,
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
            on_func,
            on_source,
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
                    .chain(on_func.into_iter().map(LoadEvent::VimFunc))
                    .chain(on_source.into_iter().map(LoadEvent::OnSource))
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
                on_func: Vec::new(),
                on_source: Vec::new(),
                on_map: KeyPattern::default(),
            },
            LazyType::Opt(events) => {
                let mut on_event = Vec::new();
                let mut on_cmd = Vec::new();
                let mut on_ft = Vec::new();
                let mut on_func = Vec::new();
                let mut on_source = Vec::new();
                let mut on_map = KeyPattern::default();

                for event in events {
                    match event {
                        LoadEvent::Autocmd(a) => on_event.push(a),
                        LoadEvent::UserCmd(u) => on_cmd.push(u),
                        LoadEvent::FileType(f) => on_ft.push(f),
                        LoadEvent::VimFunc(f) => on_func.push(f),
                        LoadEvent::OnSource(source) => on_source.push(source),
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
                    on_func,
                    on_source,
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
    pub depends: Vec<String>,
    #[serde(rename = "name")]
    pub custom_name: Option<String>,
    #[serde(flatten)]
    #[serde_as(as = "FromInto<SetupScriptOne>")]
    pub script: SetupScript,
    #[serde(flatten)]
    pub merge: MergeConfig,
}

impl PluginConfig {
    pub(super) fn dep_name(&self) -> Option<&str> {
        if let Some(custom_name) = &self.custom_name {
            return Some(custom_name);
        }
        match &self.cache.repo {
            Some(RepoSource::GitHub { repo, .. }) => Some(repo.as_ref()),
            Some(RepoSource::Git { url, .. }) => Some(repo_basename(url.as_ref())),
            None => None,
        }
    }
}

/// Git URL のクローン時に作成されるディレクトリ名（最後のパスセグメント）を返す。
/// `git clone` と同じく末尾の `.git` と末尾スラッシュを取り除いてから最後の `/` 以降を取る。
/// 例: `https://gitlab.com/foo/bar.nvim.git` → `bar.nvim`
fn repo_basename(url: &str) -> &str {
    // 末尾の `.git` と `/` を（どちらの順序で現れても）取り除く。
    let mut s = url;
    while let Some(stripped) = s.strip_suffix(".git").or_else(|| s.strip_suffix('/')) {
        s = stripped;
    }
    s.rsplit('/').next().unwrap_or(s)
}

impl DagNode for PluginConfig {
    fn id(&self) -> Option<&str> {
        self.dep_name()
    }
    fn depends(&self) -> impl IntoIterator<Item = &impl AsRef<str>> {
        &self.depends
    }
}

/// プラグインのセットアップに用いるスクリプト群
#[derive(Deserialize, Default)]
struct SetupScriptOne {
    /// Neovim 起動時に実行される Lua スクリプト
    lua_start: Option<String>,
    /// プラグイン読み込み直後に実行される Lua スクリプト
    lua_after: Option<String>,
    /// プラグイン読み込み直前に実行される Lua スクリプト
    lua_before: Option<String>,
}

/// プラグインのセットアップに用いるスクリプト群
#[derive(Clone, Default, Debug, Hash, PartialEq, Eq)]
pub struct SetupScript {
    /// Neovim 起動時に実行される Lua スクリプト
    pub lua_start: BTreeSet<String>,
    /// プラグイン読み込み直後に実行される Lua スクリプト
    pub lua_after: BTreeSet<String>,
    /// プラグイン読み込み直前に実行される Lua スクリプト
    pub lua_before: BTreeSet<String>,
}

impl From<SetupScriptOne> for SetupScript {
    fn from(value: SetupScriptOne) -> Self {
        let SetupScriptOne {
            lua_start,
            lua_after,
            lua_before,
        } = value;
        SetupScript {
            lua_start: lua_start.into_iter().collect(),
            lua_after: lua_after.into_iter().collect(),
            lua_before: lua_before.into_iter().collect(),
        }
    }
}

impl AddAssign for SetupScript {
    fn add_assign(&mut self, rhs: Self) {
        self.lua_start.extend(rhs.lua_start);
        self.lua_after.extend(rhs.lua_after);
        self.lua_before.extend(rhs.lua_before);
    }
}

/// プラグインのセットアップに用いるスクリプト群
#[derive(Deserialize)]
pub struct MergeConfig {
    #[serde(deserialize_with = "deserialize_file_specifier")]
    #[serde(default = "default_ignore")]
    pub ignore: FileSpecifier,
    #[serde(default = "default_merge_true")]
    pub merge: bool,
}

fn default_merge_true() -> bool {
    true
}

fn default_ignore() -> FileSpecifier {
    include_str!("../../../templates/ignore.gitignore")
        .parse()
        .unwrap()
}

fn deserialize_file_specifier<'de, D>(deserializer: D) -> Result<FileSpecifier, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    Ok(s.parse().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_config_deserializes_lua_start() {
        let config: Config = toml::from_str(
            r#"
            [[plugins]]
            repo = "owner/plugin"
            lua_start = "vim.g.rsplug_lua_start = true"
            lua_before = "vim.g.rsplug_before = true"
            lua_after = "vim.g.rsplug_after = true"
            "#,
        )
        .unwrap();

        let script = &config.plugins[0].script;
        assert!(script.lua_start.contains("vim.g.rsplug_lua_start = true"));
        assert!(script.lua_before.contains("vim.g.rsplug_before = true"));
        assert!(script.lua_after.contains("vim.g.rsplug_after = true"));
    }

    #[test]
    fn plugin_config_deserializes_on_func() {
        let config: Config = toml::from_str(
            r#"
            [[plugins]]
            repo = "owner/plugin"
            on_func = ["MyFunc", "autoload#Func"]
            "#,
        )
        .unwrap();

        let LazyType::Opt(events) = &config.plugins[0].lazy_type else {
            panic!("expected opt")
        };
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LoadEvent::VimFunc(_)))
        );
    }

    #[test]
    fn plugin_config_allows_script_only_entry_without_repo() {
        let config: Config = toml::from_str(
            r#"
            [[plugins]]
            lua_start = "vim.g.rsplug_script_only = true"
            "#,
        )
        .unwrap();

        assert!(config.plugins[0].cache.repo.is_none());
        assert_eq!(config.plugins[0].dep_name(), None);
        assert!(
            config.plugins[0]
                .script
                .lua_start
                .contains("vim.g.rsplug_script_only = true")
        );
    }

    #[test]
    fn plugin_config_deserializes_lua_post_update() {
        let config: Config = toml::from_str(
            r#"
            [[plugins]]
            repo = "owner/plugin"
            lua_post_update = "vim.g.updated = true"
            "#,
        )
        .unwrap();

        assert_eq!(
            config.plugins[0].cache.lua_post_update.as_deref(),
            Some("vim.g.updated = true")
        );
    }
    #[test]
    fn plugin_config_deserializes_on_source() {
        let config: Config = toml::from_str(
            r#"
            [[plugins]]
            repo = "owner/plugin"
            on_source = "host.nvim"
            "#,
        )
        .unwrap();

        let LazyType::Opt(events) = &config.plugins[0].lazy_type else {
            panic!("expected opt")
        };
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LoadEvent::OnSource(source) if source == "host.nvim"))
        );
    }

    #[test]
    fn repo_basename_extracts_clone_directory_name() {
        assert_eq!(
            repo_basename("https://gitlab.com/foo/bar.nvim.git"),
            "bar.nvim"
        );
        assert_eq!(repo_basename("https://gitlab.com/foo/bar.nvim"), "bar.nvim");
        assert_eq!(
            repo_basename("https://gitlab.com/foo/bar.nvim/"),
            "bar.nvim"
        );
        assert_eq!(
            repo_basename("https://gitlab.com/foo/bar.nvim.git/"),
            "bar.nvim"
        );
        assert_eq!(repo_basename("git@gitlab.com:foo/bar.nvim.git"), "bar.nvim");
        assert_eq!(repo_basename("https://gitlab.com"), "gitlab.com");
    }

    #[test]
    fn dep_name_uses_repo_basename_for_git_url() {
        let config: Config = toml::from_str(
            r#"
            [[plugins]]
            repo = "https://gitlab.com/owner/plugin.nvim.git"
            "#,
        )
        .unwrap();
        assert_eq!(config.plugins[0].dep_name(), Some("plugin.nvim"));
    }

    #[test]
    fn dep_name_custom_name_overrides_repo_basename() {
        let config: Config = toml::from_str(
            r#"
            [[plugins]]
            repo = "https://gitlab.com/owner/plugin.nvim.git"
            name = "my-plugin"
            "#,
        )
        .unwrap();
        assert_eq!(config.plugins[0].dep_name(), Some("my-plugin"));
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

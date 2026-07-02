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
    #[serde(rename = "repo")]
    pub repo: RepoSource,
    #[serde(default, rename = "sym")]
    pub manually_to_sym: bool,
    #[serde(default)]
    pub build: Vec<String>,
    #[serde(default)]
    pub lua_build: Option<String>,
    #[serde(default)]
    pub lua_post_update: Option<String>,
}

impl CacheConfig {
    pub fn to_sym(&self, lazy_type: &LazyType) -> bool {
        self.manually_to_sym
            || !self.build.is_empty()
            || self.lua_build.is_some()
            || self.lua_post_update.is_some()
            || matches!(lazy_type, LazyType::Opt(events) if events.iter().any(|event| matches!(event, LoadEvent::OnSource(_))))
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

impl DagNode for PluginConfig {
    fn id(&self) -> &str {
        self.custom_name.as_ref().map_or(
            match &self.cache.repo {
                RepoSource::GitHub { repo, .. } => repo.as_ref(),
                RepoSource::Git { url, .. } => url.as_ref(),
            },
            |v| v,
        )
    }
    fn depends(&self) -> impl IntoIterator<Item = &impl AsRef<str>> {
        &self.depends
    }
}

/// プラグインのセットアップに用いるスクリプト群
#[derive(Default)]
struct SetupScriptOne {
    /// Neovim 起動時に実行される Lua スクリプト
    lua_start: Option<String>,
    /// プラグイン読み込み直後に実行される Lua スクリプト
    lua_after: Option<String>,
    /// プラグイン読み込み直前に実行される Lua スクリプト
    lua_before: Option<String>,
    /// `lua_{autocmd}` 形式の autocmd Lua スクリプト
    lua_autocmd: BTreeMap<String, String>,
}

// 手動 Deserialize: `lua_*` キーだけを取り込み、それ以外(`merge`/`ignore`/`repo`/...)
// は無視する。かつて `#[serde(flatten)] BTreeMap<String,String>` が兄弟フィールドを
// 総取りで飲み込んでいたのを防ぐため、明示的にプレフィックスで絞る。
impl<'de> Deserialize<'de> for SetupScriptOne {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::{IgnoredAny, MapAccess, Visitor};
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = SetupScriptOne;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("plugin script fields")
            }
            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut lua_start = None;
                let mut lua_after = None;
                let mut lua_before = None;
                let mut lua_autocmd: BTreeMap<String, String> = BTreeMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "lua_start" => lua_start = Some(map.next_value()?),
                        "lua_after" => lua_after = Some(map.next_value()?),
                        "lua_before" => lua_before = Some(map.next_value()?),
                        k if k.starts_with("lua_") => {
                            lua_autocmd.insert(k.to_string(), map.next_value()?);
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok(SetupScriptOne {
                    lua_start,
                    lua_after,
                    lua_before,
                    lua_autocmd,
                })
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// プラグインのセットアップに用いるスクリプト群
#[derive(Clone, Default, Debug)]
pub struct SetupScript {
    /// Neovim 起動時に実行される Lua スクリプト
    pub lua_start: BTreeSet<String>,
    /// プラグイン読み込み直後に実行される Lua スクリプト
    pub lua_after: BTreeSet<String>,
    /// プラグイン読み込み直前に実行される Lua スクリプト
    pub lua_before: BTreeSet<String>,
    /// autocmd 発火時に実行される Lua スクリプト
    pub lua_autocmd: BTreeMap<Autocmd, BTreeSet<String>>,
}

impl From<SetupScriptOne> for SetupScript {
    fn from(value: SetupScriptOne) -> Self {
        let SetupScriptOne {
            lua_start,
            lua_after,
            lua_before,
            lua_autocmd,
        } = value;
        let lua_autocmd = lua_autocmd
            .into_iter()
            .filter_map(|(key, script)| {
                let event = key.strip_prefix("lua_")?;
                if matches!(event, "start" | "before" | "after" | "build") {
                    return None;
                }
                Some((event.parse::<Autocmd>().ok()?, script))
            })
            .fold(
                BTreeMap::<Autocmd, BTreeSet<String>>::new(),
                |mut acc, (event, script)| {
                    acc.entry(event).or_default().insert(script);
                    acc
                },
            );
        SetupScript {
            lua_start: lua_start.into_iter().collect(),
            lua_after: lua_after.into_iter().collect(),
            lua_before: lua_before.into_iter().collect(),
            lua_autocmd,
        }
    }
}

impl AddAssign for SetupScript {
    fn add_assign(&mut self, rhs: Self) {
        self.lua_start.extend(rhs.lua_start);
        self.lua_after.extend(rhs.lua_after);
        self.lua_before.extend(rhs.lua_before);
        for (event, scripts) in rhs.lua_autocmd {
            self.lua_autocmd.entry(event).or_default().extend(scripts);
        }
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
    fn plugin_merge_and_ignore_parse_without_being_swallowed_by_lua_catchall() {
        // ponytail: guards against the greedy `#[serde(flatten)] BTreeMap<String,String>`
        // catch-all in SetupScriptOne swallowing sibling keys like `merge`/`ignore`.
        let config: Config = toml::from_str(
            r#"
            [[plugins]]
            repo = "owner/plugin"
            sym = true
            start = true
            merge = false
            ignore = "*.tmp"
            lua_User = "vim.g.x = true"
            "#,
        )
        .unwrap();
        let p = &config.plugins[0];
        assert!(!p.merge.merge, "merge bool must reach MergeConfig");
        assert_eq!(
            p.merge.ignore.matched("foo.tmp"),
            true,
            "ignore must reach MergeConfig, not be swallowed"
        );
        assert!(
            p.script.lua_autocmd.len() == 1
                && p.script.lua_autocmd.values().flatten().any(|s| s.contains("vim.g.x")),
            "lua_User must still reach the autocmd script map"
        );
        assert!(config.plugins[0].cache.to_sym(&config.plugins[0].lazy_type));
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

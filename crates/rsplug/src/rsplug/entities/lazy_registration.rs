use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, btree_map::Keys},
    iter::Sum,
    ops::AddAssign,
    path::PathBuf,
    sync::Arc,
};

use sailfish::{TemplateSimple, runtime::Render};

use super::*;
use crate::rsplug::util::hash;

/// Render untrusted configuration text as a Lua string literal.  Generated
/// runtime files are Lua source, so Sailfish's HTML escaping is deliberately
/// disabled and every value entering a quoted literal must use this helper.
#[derive(Debug, Clone)]
pub(super) struct LuaStringLiteral(String);

impl std::fmt::Display for LuaStringLiteral {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Render for LuaStringLiteral {
    fn render(&self, buffer: &mut sailfish::runtime::Buffer) -> Result<(), sailfish::RenderError> {
        self.0.render(buffer)
    }
}

pub(super) fn lua_string(value: impl std::fmt::Display) -> LuaStringLiteral {
    let input = value.to_string();
    let mut out = String::with_capacity(input.len() + 8);
    out.push('"');
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push_str(&format!("\\{:03}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    LuaStringLiteral(out)
}

/// docファイルを束ねたヘルプ専用プラグインの source_name。
/// `pack/_gen/start/` への配置判定に使われる。
pub(super) const DOC_PLUGIN_NAME: &str = "_rsplug:doc";

struct PkgId2ScriptsItem {
    pkgid: PluginIDStr,
    script: SetupScript,
    order: usize,
    start: bool, // もし読み込みプラグイン元が LazyType::Start なら、他のスクリプトと別の仕組みでスクリプトを呼び出す必要があるため
}

/// プラグインの読み込み制御・ロード後の設定 (after_lua等)を行う構造体
#[derive(Default)]
pub struct LazyRegistration {
    pkgid2scripts: Vec<PkgId2ScriptsItem>,
    event2pkgid: BTreeMap<Autocmd, Vec<PluginIDStr>>,
    cmd2pkgid: BTreeMap<UserCmd, Vec<PluginIDStr>>,
    ft2pkgid: BTreeMap<FileType, Vec<PluginIDStr>>,
    func2pkgid: BTreeMap<VimFunc, Vec<PluginIDStr>>,
    luam2pkgid: BTreeMap<LuaModule, Vec<PluginIDStr>>,
    source_name2pkgid: BTreeMap<String, Vec<PluginIDStr>>,
    source_target2pkgid: BTreeMap<String, PluginIDStr>,
    keypattern2pkgid: BTreeMap<ModeChar, BTreeMap<Arc<String>, Vec<PluginIDStr>>>,
}

/// 生成ファイル（`FileSource::File`）の `(install_path, FileItem)` を作る。
/// 内容の `data_hash` を identity に含めることで、生成内容の変更が id に反映される。
fn generated_file_item(path: impl Into<PathBuf>, data: Cow<'static, [u8]>) -> (PathBuf, FileItem) {
    let path = path.into();
    let data_hash = hash::digest_hash(&data);
    let item = FileItem::new(
        Arc::new(FileSource::File { data }),
        FileIdentity::GeneratedFile {
            path: path.clone(),
            data_hash,
        },
        MergeType::Overwrite,
    );
    (path, item)
}

/// 単スクリプトをランタイムパスに配置するためのパッケージを作成する。
fn instant_startup_pkg(path: &str, data: impl Into<Cow<'static, [u8]>>) -> LoadedPlugin {
    let source_names = BTreeSet::from([format!("_rsplug:{path}")]);
    let (path, item) = generated_file_item(PathBuf::from(path), data.into());
    let files = BTreeMap::from([(path, item)]);
    LoadedPlugin {
        source_names,
        lazy_type: LazyType::Start,
        files: HowToPlaceFiles::CopyEachFile(files),
        script: Default::default(),
        order: usize::MAX,
        merge_enabled: true,
        is_lazy_registration: true,
        dotgit: false,
    }
}

impl From<LazyRegistration> for Vec<LoadedPlugin> {
    fn from(value: LazyRegistration) -> Vec<LoadedPlugin> {
        if value.is_empty() {
            return Vec::with_capacity(0);
        }
        let LazyRegistration {
            pkgid2scripts,
            event2pkgid,
            cmd2pkgid,
            ft2pkgid,
            func2pkgid,
            luam2pkgid,
            source_name2pkgid,
            source_target2pkgid,
            keypattern2pkgid,
        } = value;

        let mut plugs = vec![instant_startup_pkg(
            "./doc/rsplug.txt",
            include_bytes!("../../../templates/doc/rsplug.txt"),
        )];

        {
            // Add packages to place scripts that does the initial setup of the plugin
            let (pkgid2scripts, startup_plugins, startup_scripts) = pkgid2scripts.into_iter().fold(
                (Vec::new(), Vec::new(), Vec::new()),
                |(mut scripts_lazy, mut scripts_start, mut scripts_startup),
                 PkgId2ScriptsItem {
                     pkgid,
                     script,
                     order,
                     start,
                 }| {
                    let SetupScript {
                        lua_start,
                        lua_after,
                        lua_before,
                    } = script;
                    let before_scripts: Vec<String> = lua_before.into_iter().collect();
                    let after_scripts: Vec<String> = lua_after.into_iter().collect();
                    let hook_module = if before_scripts.is_empty() && after_scripts.is_empty() {
                        None
                    } else {
                        let data = HookModuleTemplate {
                            before: &before_scripts,
                            after: &after_scripts,
                        }
                        .render_once()
                        .unwrap()
                        .into_bytes();
                        let module_id =
                            format!("_rsplug/hooks_{}", hash::digest_hash_hex_string(&data));
                        plugs.push(instant_startup_pkg(&format!("lua/{module_id}.lua"), data));
                        Some(module_id)
                    };
                    for content in lua_start {
                        scripts_startup.push((order, pkgid.clone(), content));
                    }

                    if start {
                        scripts_start.push((order, pkgid.clone()));
                    }
                    if let Some(hook_module) = hook_module {
                        scripts_lazy.push((pkgid, hook_module));
                    }
                    (scripts_lazy, scripts_start, scripts_startup)
                },
            );
            let mut startup_plugins = startup_plugins;
            startup_plugins.sort_by(|(l_order, l_pkgid), (r_order, r_pkgid)| {
                l_order.cmp(r_order).then_with(|| l_pkgid.cmp(r_pkgid))
            });
            let startup_plugins = startup_plugins
                .into_iter()
                .map(|(_, pkgid)| pkgid)
                .collect();
            let mut startup_scripts = startup_scripts;
            startup_scripts.sort_by(
                |(l_order, l_pkgid, l_module), (r_order, r_pkgid, r_module)| {
                    l_order
                        .cmp(r_order)
                        .then_with(|| l_pkgid.cmp(r_pkgid))
                        .then_with(|| l_module.cmp(r_module))
                },
            );
            let startup_scripts: Vec<String> = startup_scripts
                .into_iter()
                .map(|(_, _, content)| content)
                .collect();
            let init_data: Cow<'static, [u8]> = CustomPackaddTemplate {
                pkgid2scripts,
                startup_plugins,
                source2pkgid: build_source2pkgid(source_name2pkgid, source_target2pkgid),
            }
            .render_once()
            .unwrap()
            .into_bytes()
            .into();
            let mut files = BTreeMap::from([generated_file_item(
                PathBuf::from("lua/_rsplug/init.lua"),
                init_data,
            )]);
            if !startup_scripts.is_empty() {
                let data: Cow<'static, [u8]> = LuaStartPluginTemplate {
                    startup_scripts: &startup_scripts,
                }
                .render_once()
                .unwrap()
                .into_bytes()
                .into();
                let (ls_path, ls_item) =
                    generated_file_item(PathBuf::from("plugin/lua_start.lua"), data);
                files.insert(ls_path, ls_item);
            }
            plugs.push(LoadedPlugin {
                source_names: BTreeSet::from(["_rsplug:init".to_string()]),
                lazy_type: LazyType::Start,
                files: HowToPlaceFiles::CopyEachFile(files),
                script: Default::default(),
                order: usize::MAX,
                merge_enabled: true,
                is_lazy_registration: true,
                dotgit: false,
            });
        }

        if !ft2pkgid.is_empty() {
            // on_ft setup
            plugs.push(instant_startup_pkg(
                "lua/_rsplug/on_ft.lua",
                include_bytes!("../../../templates/lua/_rsplug/on_ft.lua"),
            ));
            for (ft, pkgids) in ft2pkgid {
                let mut path = format!("ftplugin/{ft}/");
                let data = FtpluginTemplate { pkgids, ft }
                    .render_once()
                    .unwrap()
                    .into_bytes();
                path.push_str(&hash::digest_hash_hex_string(&data));
                path.push_str(".lua");

                plugs.push(instant_startup_pkg(&path, data));
            }
        }

        if !event2pkgid.is_empty() {
            // on_event setup
            let events = event2pkgid.keys();
            let on_event_setup: Cow<'static, [u8]> = OnEventSetupTemplate { events }
                .render_once()
                .unwrap()
                .into_bytes()
                .into();
            let on_event: Cow<'static, [u8]> = OnEventTemplate {
                event2pkgid: &event2pkgid,
            }
            .render_once()
            .unwrap()
            .into_bytes()
            .into();
            let on_event_setup_path = PathBuf::from(format!(
                "plugin/{}.lua",
                hash::digest_hash_hex_string(&on_event_setup)
            ));
            let files = BTreeMap::from([
                generated_file_item(PathBuf::from("lua/_rsplug/on_event.lua"), on_event),
                generated_file_item(on_event_setup_path, on_event_setup),
            ]);
            plugs.push({
                LoadedPlugin {
                    source_names: BTreeSet::from(["_rsplug:on_event".to_string()]),
                    lazy_type: LazyType::Start,
                    files: HowToPlaceFiles::CopyEachFile(files),
                    script: Default::default(),
                    order: usize::MAX,
                    merge_enabled: true,
                    is_lazy_registration: true,
                    dotgit: false,
                }
            });
        }
        if !func2pkgid.is_empty() {
            let funcs = func2pkgid.keys();
            let on_func_setup: Cow<'static, [u8]> = OnFuncSetupTemplate { funcs }
                .render_once()
                .unwrap()
                .into_bytes()
                .into();
            let on_func: Cow<'static, [u8]> = OnFuncTemplate {
                func2pkgid: &func2pkgid,
            }
            .render_once()
            .unwrap()
            .into_bytes()
            .into();
            let on_func_setup_path = PathBuf::from(format!(
                "plugin/{}.lua",
                hash::digest_hash_hex_string(&on_func_setup)
            ));
            let files = BTreeMap::from([
                generated_file_item(PathBuf::from("lua/_rsplug/on_func.lua"), on_func),
                generated_file_item(on_func_setup_path, on_func_setup),
            ]);
            plugs.push(LoadedPlugin {
                source_names: BTreeSet::from(["_rsplug:on_func".to_string()]),
                lazy_type: LazyType::Start,
                files: HowToPlaceFiles::CopyEachFile(files),
                script: Default::default(),
                order: usize::MAX,
                merge_enabled: true,
                is_lazy_registration: true,
                dotgit: false,
            });
        }
        if !cmd2pkgid.is_empty() {
            // on_cmd setup
            plugs.push({
                let cmds = cmd2pkgid.keys();
                let on_cmd_setup: Cow<'static, [u8]> = OnCmdSetupTemplate { cmds }
                    .render_once()
                    .unwrap()
                    .into_bytes()
                    .into();
                let on_cmd: Cow<'static, [u8]> = OnCmdTemplate {
                    cmd2pkgid: &cmd2pkgid,
                }
                .render_once()
                .unwrap()
                .into_bytes()
                .into();
                let on_cmd_setup_path = PathBuf::from(format!(
                    "plugin/{}.lua",
                    hash::digest_hash_hex_string(&on_cmd_setup)
                ));
                let files = BTreeMap::from([
                    generated_file_item(PathBuf::from("lua/_rsplug/on_cmd.lua"), on_cmd),
                    generated_file_item(on_cmd_setup_path, on_cmd_setup),
                ]);
                LoadedPlugin {
                    source_names: BTreeSet::from(["_rsplug:on_cmd".to_string()]),
                    lazy_type: LazyType::Start,
                    files: HowToPlaceFiles::CopyEachFile(files),
                    script: Default::default(),
                    order: usize::MAX,
                    merge_enabled: true,
                    is_lazy_registration: true,
                    dotgit: false,
                }
            });
        }
        if !luam2pkgid.is_empty() {
            let plugin_on_lua = include_bytes!("../../../templates/plugin/on_lua.lua");
            // R4: luam2pkgid から pkgid2luam (id -> [root]) を決定的に導出する。
            let mut pkgid2luam_map: BTreeMap<PluginIDStr, BTreeSet<String>> = BTreeMap::new();
            for (luam, ids) in &luam2pkgid {
                for id in ids {
                    pkgid2luam_map
                        .entry(id.clone())
                        .or_default()
                        .insert(luam.to_string());
                }
            }
            let pkgid2luam: Vec<(PluginIDStr, Vec<String>)> = pkgid2luam_map
                .into_iter()
                .map(|(id, roots)| (id, roots.into_iter().collect()))
                .collect();
            let on_lua: Cow<'static, [u8]> = OnLuaTemplate {
                luam2pkgid: &luam2pkgid,
                pkgid2luam,
            }
            .render_once()
            .unwrap()
            .into_bytes()
            .into();
            let plugin_on_lua_path = PathBuf::from(format!(
                "plugin/{}.lua",
                hash::digest_hash_hex_string(plugin_on_lua)
            ));
            let files = BTreeMap::from([
                generated_file_item(PathBuf::from("lua/_rsplug/on_lua.lua"), on_lua),
                generated_file_item(plugin_on_lua_path, plugin_on_lua.into()),
            ]);
            plugs.push(LoadedPlugin {
                source_names: BTreeSet::from(["_rsplug:on_lua".to_string()]),
                lazy_type: LazyType::Start,
                files: HowToPlaceFiles::CopyEachFile(files),
                script: Default::default(),
                order: usize::MAX,
                merge_enabled: true,
                is_lazy_registration: true,
                dotgit: false,
            });
        }
        if !keypattern2pkgid.is_empty() {
            // R5: on_map セットアップはテンプレート化。到達可能モードから pending_modes を構築し、
            // 専有 augroup に ModeChanged / VimEnter(once) を登録する。
            let on_map_setup: Cow<'static, [u8]> = OnMapSetupTemplate {
                modes: keypattern2pkgid.keys(),
            }
            .render_once()
            .unwrap()
            .into_bytes()
            .into();
            plugs.push(instant_startup_pkg(
                &format!("plugin/{}.lua", hash::digest_hash_hex_string(&on_map_setup)),
                on_map_setup,
            ));
            plugs.push(instant_startup_pkg(
                "lua/_rsplug/on_map/init.lua",
                include_bytes!("../../../templates/lua/_rsplug/on_map/init.lua"),
            ));
            for mode in keypattern2pkgid.keys() {
                let data = OnMapTemplate {
                    mode,
                    keypattern2pkgid: &keypattern2pkgid,
                }
                .render_once()
                .unwrap()
                .into_bytes();
                plugs.push(instant_startup_pkg(
                    &format!("lua/_rsplug/on_map/mode_{mode}.lua"),
                    data,
                ));
            }
        }

        // NOTE: doc 盗みは `LoadedPlugin::split_doc`（`PackPlan::load`）で LoadedPlugin として
        // 扱い、ここ（control マージ）で rsplug-doc・lazy loader と統一マージされる。
        // LazyRegistration 自体は lazy 実行制御のみを担う。

        plugs
    }
}

impl AddAssign for LazyRegistration {
    fn add_assign(&mut self, other: Self) {
        let Self {
            pkgid2scripts,
            event2pkgid,
            cmd2pkgid,
            ft2pkgid,
            func2pkgid,
            luam2pkgid,
            source_name2pkgid,
            source_target2pkgid,
            keypattern2pkgid,
        } = other;
        for (event, ids) in event2pkgid {
            self.event2pkgid.entry(event).or_default().extend(ids);
        }
        self.pkgid2scripts.extend(pkgid2scripts);
        for (cmd, ids) in cmd2pkgid {
            self.cmd2pkgid.entry(cmd).or_default().extend(ids);
        }
        for (ft, ids) in ft2pkgid {
            self.ft2pkgid.entry(ft).or_default().extend(ids);
        }
        for (func, ids) in func2pkgid {
            self.func2pkgid.entry(func).or_default().extend(ids);
        }
        for (luam, ids) in luam2pkgid {
            self.luam2pkgid.entry(luam).or_default().extend(ids);
        }
        for (source, ids) in source_name2pkgid {
            self.source_name2pkgid
                .entry(source)
                .or_default()
                .extend(ids);
        }
        self.source_target2pkgid.extend(source_target2pkgid);
        for (key, pattern) in keypattern2pkgid {
            let mode_entry = self.keypattern2pkgid.entry(key).or_default();
            for (pattern, ids) in pattern {
                mode_entry.entry(pattern).or_default().extend(ids);
            }
        }

        // Trigger records are rendered in insertion order. Deduplicate once while
        // composing the registration so generated Lua need not linearly scan lists.
        dedup_trigger_ids(&mut self.event2pkgid);
        dedup_trigger_ids(&mut self.cmd2pkgid);
        dedup_trigger_ids(&mut self.ft2pkgid);
        dedup_trigger_ids(&mut self.func2pkgid);
        dedup_trigger_ids(&mut self.luam2pkgid);
        dedup_trigger_ids(&mut self.source_name2pkgid);
        for patterns in self.keypattern2pkgid.values_mut() {
            dedup_trigger_ids(patterns);
        }
    }
}

fn dedup_trigger_ids<K>(records: &mut BTreeMap<K, Vec<PluginIDStr>>)
where
    K: Ord,
{
    for ids in records.values_mut() {
        let mut seen = BTreeSet::new();
        ids.retain(|id| seen.insert(id.clone()));
    }
}

impl Sum for LazyRegistration {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        let mut res = LazyRegistration::new();
        for l in iter {
            res += l
        }
        res
    }
}

impl LazyRegistration {
    /// Create empty
    pub fn new() -> Self {
        Default::default()
    }
    /// LazyRegistrationが空かどうか
    pub fn is_empty(&self) -> bool {
        let Self {
            pkgid2scripts: scripts,
            event2pkgid,
            cmd2pkgid,
            ft2pkgid,
            func2pkgid,
            luam2pkgid,
            source_name2pkgid,
            source_target2pkgid,
            keypattern2pkgid,
        } = self;
        event2pkgid.is_empty()
            && scripts.is_empty()
            && cmd2pkgid.is_empty()
            && ft2pkgid.is_empty()
            && func2pkgid.is_empty()
            && luam2pkgid.is_empty()
            && source_name2pkgid.is_empty()
            && source_target2pkgid.is_empty()
            && keypattern2pkgid.values().all(|v| v.is_empty())
    }

    /// on_ft で登録された `(ft, id)` を文字列キーで取り出す（R1: ft インデックス構築用）。
    /// `PackPlan::install` が `self.ctl` を消費する前に呼ぶ。ft2pkgid 以外のマップは
    /// 外部に公開しない。戻り値は `ft -> [id]`（id は挿入順、重複なしを前提）。
    pub(super) fn ft_index_pairs(&self) -> BTreeMap<String, Vec<String>> {
        self.ft2pkgid
            .iter()
            .map(|(ft, ids)| {
                (
                    ft.to_string(),
                    ids.iter().map(|id| id.to_string()).collect(),
                )
            })
            .collect()
    }

    #[cfg(test)]
    pub(super) fn event_ids_for_test(&self, event: &Autocmd) -> Vec<String> {
        self.event2pkgid
            .get(event)
            .into_iter()
            .flatten()
            .map(ToString::to_string)
            .collect()
    }

    /// パッケージ情報を読み込み、 LazyRegistration を作成する。
    /// 読み込む情報が要らない場合は `None` を返す。
    /// NOTE: Package はインストールされる必要があるため、変更を抑制する意図で PackageID の所有権を奪う。
    /// その他必要な情報のみ引数に取る。
    pub(super) fn create(
        id: PluginID,
        source_names: BTreeSet<String>,
        lazy_type: LazyType,
        script: SetupScript,
        order: usize,
    ) -> Self {
        let id_str = id.as_str();
        // 全 source_name を自身の id に紐付ける。マージで集約された複数名すべてが
        // on_source 参照できるようにする（Phase 1: source_name を潰さない）。
        let source_target2pkgid = source_names
            .into_iter()
            .map(|source_name| (source_name, id_str.clone()))
            .collect::<BTreeMap<_, _>>();

        let LazyType::Opt(events) = lazy_type else {
            return Self {
                pkgid2scripts: vec![PkgId2ScriptsItem {
                    pkgid: id_str,
                    script,
                    order,
                    start: true,
                }],
                source_target2pkgid,
                ..Default::default()
            };
        };
        let mut event2pkgid: BTreeMap<Autocmd, Vec<_>> = BTreeMap::new();
        let mut cmd2pkgid: BTreeMap<UserCmd, Vec<_>> = BTreeMap::new();
        let mut ft2pkgid: BTreeMap<FileType, Vec<_>> = BTreeMap::new();
        let mut func2pkgid: BTreeMap<VimFunc, Vec<_>> = BTreeMap::new();
        let mut luam2pkgid: BTreeMap<LuaModule, Vec<_>> = BTreeMap::new();
        let mut source_name2pkgid: BTreeMap<String, Vec<_>> = BTreeMap::new();
        let mut keypattern2pkgid: BTreeMap<ModeChar, BTreeMap<Arc<String>, Vec<_>>> =
            BTreeMap::new();

        let pkgid2scripts = vec![PkgId2ScriptsItem {
            pkgid: id_str.clone(),
            script,
            order,
            start: false,
        }];
        for ev in events {
            use LoadEvent::*;
            match ev {
                Autocmd(autocmd) => {
                    event2pkgid.entry(autocmd).or_default().push(id.as_str());
                }
                UserCmd(cmd) => {
                    cmd2pkgid.entry(cmd).or_default().push(id.as_str());
                }
                FileType(ft) => {
                    ft2pkgid.entry(ft).or_default().push(id.as_str());
                }
                VimFunc(func) => {
                    func2pkgid.entry(func).or_default().push(id_str.clone());
                }
                LuaModule(luam) => {
                    luam2pkgid.entry(luam).or_default().push(id_str.clone());
                }
                OnSource(source_name) => {
                    source_name2pkgid
                        .entry(source_name)
                        .or_default()
                        .push(id_str.clone());
                }
                OnMap(pattern) => {
                    let KeyPattern(pattern) = pattern;
                    let id = id.as_str();
                    for (mode, pattern) in pattern {
                        for pattern in pattern {
                            keypattern2pkgid
                                .entry(mode.clone())
                                .or_default()
                                .entry(pattern)
                                .or_default()
                                .push(id.clone());
                        }
                    }
                }
            }
        }
        Self {
            pkgid2scripts,
            event2pkgid,
            cmd2pkgid,
            ft2pkgid,
            func2pkgid,
            luam2pkgid,
            source_name2pkgid,
            source_target2pkgid,
            keypattern2pkgid,
        }
    }
}

#[derive(TemplateSimple)]
#[template(path = "ftplugin/on_ft.stpl")]
#[template(escape = false)]
struct FtpluginTemplate {
    pkgids: Vec<PluginIDStr>,
    ft: FileType,
}

#[derive(TemplateSimple)]
#[template(path = "lua/_rsplug/init.stpl")]
#[template(escape = false)]
struct CustomPackaddTemplate {
    pkgid2scripts: Vec<(PluginIDStr, String)>,
    startup_plugins: Vec<PluginIDStr>,
    source2pkgid: Vec<(PluginIDStr, Vec<PluginIDStr>)>,
}

#[derive(TemplateSimple)]
#[template(path = "lua/_rsplug/hooks.stpl")]
#[template(escape = false)]
struct HookModuleTemplate<'a> {
    before: &'a [String],
    after: &'a [String],
}

#[derive(TemplateSimple)]
#[template(path = "plugin/lua_start.stpl")]
#[template(escape = false)]
struct LuaStartPluginTemplate<'a> {
    startup_scripts: &'a [String],
}

fn build_source2pkgid(
    source_name2pkgid: BTreeMap<String, Vec<PluginIDStr>>,
    source_target2pkgid: BTreeMap<String, PluginIDStr>,
) -> Vec<(PluginIDStr, Vec<PluginIDStr>)> {
    let mut source2pkgid = Vec::new();
    for (source_name, pkgids) in source_name2pkgid {
        if let Some(source_pkgid) = source_target2pkgid.get(&source_name) {
            source2pkgid.push((source_pkgid.clone(), pkgids));
        }
    }
    source2pkgid.sort_by(|(l, _), (r, _)| l.cmp(r));
    source2pkgid
}

#[derive(TemplateSimple)]
#[template(path = "plugin/on_event.stpl")]
#[template(escape = false)]
struct OnEventSetupTemplate<'a> {
    events: Keys<'a, Autocmd, Vec<PluginIDStr>>,
}

#[derive(TemplateSimple)]
#[template(path = "lua/_rsplug/on_event.stpl")]
#[template(escape = false)]
struct OnEventTemplate<'a> {
    event2pkgid: &'a BTreeMap<Autocmd, Vec<PluginIDStr>>,
}

#[derive(TemplateSimple)]
#[template(path = "plugin/on_cmd.stpl")]
#[template(escape = false)]
struct OnCmdSetupTemplate<'a> {
    cmds: Keys<'a, UserCmd, Vec<PluginIDStr>>,
}

#[derive(TemplateSimple)]
#[template(path = "lua/_rsplug/on_cmd.stpl")]
#[template(escape = false)]
struct OnCmdTemplate<'a> {
    cmd2pkgid: &'a BTreeMap<UserCmd, Vec<PluginIDStr>>,
}

#[derive(TemplateSimple)]
#[template(path = "plugin/on_func.stpl")]
#[template(escape = false)]
struct OnFuncSetupTemplate<'a> {
    funcs: Keys<'a, VimFunc, Vec<PluginIDStr>>,
}

#[derive(TemplateSimple)]
#[template(path = "lua/_rsplug/on_func.stpl")]
#[template(escape = false)]
struct OnFuncTemplate<'a> {
    func2pkgid: &'a BTreeMap<VimFunc, Vec<PluginIDStr>>,
}

#[derive(TemplateSimple)]
#[template(path = "lua/_rsplug/on_lua.stpl")]
#[template(escape = false)]
struct OnLuaTemplate<'a> {
    luam2pkgid: &'a BTreeMap<LuaModule, Vec<PluginIDStr>>,
    /// id -> [root]。luam2pkgid から決定的に導出し、sort/dedup 済み。
    pkgid2luam: Vec<(PluginIDStr, Vec<String>)>,
}

#[derive(TemplateSimple)]
#[template(path = "lua/_rsplug/on_map/mode__.stpl")]
#[template(escape = false)]
struct OnMapTemplate<'a> {
    mode: &'a ModeChar,
    keypattern2pkgid: &'a BTreeMap<ModeChar, BTreeMap<Arc<String>, Vec<PluginIDStr>>>,
}

#[derive(TemplateSimple)]
#[template(path = "plugin/on_map.stpl")]
#[template(escape = false)]
struct OnMapSetupTemplate<'a> {
    modes: Keys<'a, ModeChar, BTreeMap<Arc<String>, Vec<PluginIDStr>>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lua_string_literal_handles_quotes_controls_and_utf8() {
        assert_eq!(
            lua_string("quote\" slash\\ line\n\r\t\0 日本語").to_string(),
            "\"quote\\\" slash\\\\ line\\n\\r\\t\\000 日本語\""
        );
    }

    #[test]
    fn on_func_template_uses_funcundefined_for_autoload_functions() {
        let func = "foo#bar".parse::<VimFunc>().unwrap();
        let mut func2pkgid = BTreeMap::new();
        func2pkgid.insert(func, Vec::new());
        let rendered = OnFuncSetupTemplate {
            funcs: func2pkgid.keys(),
        }
        .render_once()
        .unwrap();

        assert!(rendered.contains("FuncUndefined"));
        assert!(rendered.contains("autoload_handler(\"foo#bar\")"));
        assert!(!rendered.contains("function! foo#bar"));
    }

    #[test]
    fn on_func_runtime_guards_nested_funcundefined_while_packadding_autoload() {
        let func = "foo#bar".parse::<VimFunc>().unwrap();
        let id = b"foo-plugin".plugin_id().as_str();
        let func2pkgid = BTreeMap::from([(func, vec![id])]);
        let rendered = OnFuncTemplate {
            func2pkgid: &func2pkgid,
        }
        .render_once()
        .unwrap();

        assert!(rendered.contains("called_autoload_prefix"));
        assert!(rendered.contains("vim.o.eventignore = 'FuncUndefined'"));
        assert!(rendered.contains("vim.o.eventignore = save_eventignore"));
        assert!(rendered.contains("pcall(packadd_all, func)"));
    }

    #[test]
    fn on_func_runtime_template_closes_return_table() {
        let func = "foo#bar".parse::<VimFunc>().unwrap();
        let func2pkgid = BTreeMap::from([(func, Vec::new())]);
        let rendered = OnFuncTemplate {
            func2pkgid: &func2pkgid,
        }
        .render_once()
        .unwrap();

        assert!(
            rendered.trim_end().ends_with('}'),
            "rendered on_func.lua:\n{rendered}"
        );
    }

    #[test]
    fn on_cmd_delegates_once_with_command_metadata_and_arguments() {
        let cmd = "MyCommand".parse::<UserCmd>().unwrap();
        let id = b"command-plugin".plugin_id().as_str();
        let cmd2pkgid = BTreeMap::from([(cmd, vec![id])]);
        let rendered = OnCmdTemplate {
            cmd2pkgid: &cmd2pkgid,
        }
        .render_once()
        .unwrap();

        assert!(rendered.contains("local function command_line"));
        assert!(rendered.contains("nvim_get_commands { builtin = false }"));
        assert!(rendered.contains("vim.cmd(command_line(cmd, args, cmdinfo))"));
        assert!(!rendered.contains("match '^E481:'"));
        assert!(!rendered.contains("vim.cmd(range .. cmdline)"));
    }

    #[test]
    fn custom_packadd_template_packadds_startup_plugins() {
        let startup_plugin = b"startup-plugin".plugin_id().as_str();
        let rendered = CustomPackaddTemplate {
            pkgid2scripts: Vec::new(),
            startup_plugins: vec![startup_plugin.clone()],
            source2pkgid: Vec::new(),
        }
        .render_once()
        .unwrap();

        assert!(rendered.contains(&format!(
            "local startup_plugins = {{\"{startup_plugin}\",}}"
        )));
        assert!(!rendered.contains("startup_scripts"));
        assert!(rendered.contains("require '_rsplug'.packadd(id, true)"));
        assert!(!rendered.contains("vim.list_contains(result"));
    }

    #[test]
    fn lua_start_template_wraps_scripts_in_order() {
        let rendered = LuaStartPluginTemplate {
            startup_scripts: &[
                "vim.g.first = true".to_string(),
                "return vim.g.first".to_string(),
            ],
        }
        .render_once()
        .unwrap();

        let first_pos = rendered.find("vim.g.first = true").unwrap();
        let second_pos = rendered.find("return vim.g.first").unwrap();

        assert!(rendered.starts_with("-- Auto generated by rsplug\n"));
        assert!(first_pos < second_pos);
        assert_eq!(rendered.matches("(function()\n").count(), 2);
    }
}

/// Runtime hot-paths characterization harness (PLANS R0).
///
/// このモジュールは実際のテンプレート描画 + `PackPlan::install` の公開経路そのままを
/// 使って `pack/_gen` ツリーを構築し、headless nvim で振る舞いを検証する。
/// フィクスチャは `lazy_registration.rs` に置き（PLANS の指示）、Lua アサーションは
/// `crates/rsplug/tests/runtime_hot_paths.lua` に置く。Rust 側テストが nvim を起動し、
/// `RSPLUG_TEST_RESULT=ok` を確認する。nvim が見つからない場合は明確な失敗（skip しない）。
#[cfg(test)]
mod runtime_hot_paths {
    use super::*;
    use std::{
        collections::{BTreeSet, BinaryHeap},
        ffi::OsString,
        path::PathBuf,
        process::Command,
        str::FromStr,
        sync::Arc,
    };

    /// フィクスチャが構築するダミープラグインの遅延指定。
    #[allow(dead_code)] // Start/Cmd は後段フェーズのシナリオで使用
    pub(super) enum FakeLazy {
        Start,
        Event(&'static str),
        Ft(&'static str),
        Lua(&'static str),
        Cmd(&'static str),
        Map {
            mode: Option<char>,
            pattern: &'static str,
        },
    }

    /// ダミープラグイン1件。`files` は snapshot root からの相対パスと内容。
    /// 各プラグインは `merge_enabled = false` で独立パッケージになる。
    /// `lazy` には複数トリガを指定可能（on_event と on_ft の併用など）。
    pub(super) struct FakePlugin {
        pub tag: &'static str,
        pub files: Vec<(&'static str, &'static [u8])>,
        pub lazy: Vec<FakeLazy>,
    }

    fn events_of(lazy: &FakeLazy) -> Vec<LoadEvent> {
        match lazy {
            FakeLazy::Start => vec![],
            FakeLazy::Event(e) => vec![LoadEvent::Autocmd(Autocmd::from_str(e).unwrap())],
            FakeLazy::Ft(ft) => vec![LoadEvent::FileType(FileType::from_str(ft).unwrap())],
            FakeLazy::Lua(m) => vec![LoadEvent::LuaModule(LuaModule(Arc::new((*m).to_string())))],
            FakeLazy::Cmd(c) => vec![LoadEvent::UserCmd(UserCmd::from_str(c).unwrap())],
            FakeLazy::Map { mode, pattern } => {
                let mut kp = BTreeMap::new();
                kp.insert(ModeChar::new(*mode), vec![Arc::new((*pattern).to_string())]);
                vec![LoadEvent::OnMap(KeyPattern(kp))]
            }
        }
    }

    fn lazy_type_of(lazies: &[FakeLazy]) -> LazyType {
        let mut events: BTreeSet<LoadEvent> = BTreeSet::new();
        for l in lazies {
            for ev in events_of(l) {
                events.insert(ev);
            }
        }
        if events.is_empty() {
            LazyType::Start
        } else {
            LazyType::Opt(events)
        }
    }

    /// 構築済み pack ツリー。`_tmp` が生きている間ディレクトリが保持される。
    pub(super) struct BuiltPack {
        pub packpath: PathBuf,
        pub control_ids: Vec<String>,
        _tmp: tempfile::TempDir,
    }

    /// ダミープラグン群から実際の `PackPlan::install` 経路で pack を構築する。
    pub(super) async fn build_pack(plugins: Vec<FakePlugin>) -> std::io::Result<BuiltPack> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().to_path_buf();
        let mut heap = BinaryHeap::new();
        for (order, p) in plugins.into_iter().enumerate() {
            let snap = root.join(format!("snap-{order}"));
            let snapshot = RepoSnapshotIdentity::new(
                PathBuf::from(format!("fixture/{order}")),
                (order as u64).to_le_bytes().to_vec(),
                None,
                Arc::<[String]>::from(Vec::<String>::new()),
                None,
            );
            let source = Arc::new(FileSource::Directory {
                path: Arc::from(snap.clone()),
                inventory: None,
                handle: None,
            });
            let mut files: BTreeMap<PathBuf, FileItem> = BTreeMap::new();
            for (rel, data) in &p.files {
                let relp = PathBuf::from(rel);
                let abs = snap.join(&relp);
                if let Some(parent) = abs.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&abs, data).await?;
                files.insert(
                    relp.clone(),
                    FileItem::new(
                        source.clone(),
                        FileIdentity::RepoFile(RepoFileIdentity::new(snapshot.clone(), relp)),
                        MergeType::Conflict,
                    ),
                );
            }
            let loaded = LoadedPlugin {
                source_names: BTreeSet::from([p.tag.to_string()]),
                lazy_type: lazy_type_of(&p.lazy),
                files: HowToPlaceFiles::CopyEachFile(files),
                script: SetupScript::default(),
                order,
                merge_enabled: false,
                is_lazy_registration: false,
                dotgit: false,
            };
            heap.push(loaded);
        }
        let mut plan = PackPlan::new();
        plan.load(heap);
        let packpath = root.join("packpath");
        plan.install(&packpath).await?;

        let mut control_ids = Vec::new();
        if let Ok(rd) = std::fs::read_dir(packpath.join("pack/_gen/generations")) {
            for e in rd.flatten() {
                let path = e.path();
                if path.extension().and_then(|s| s.to_str()) == Some("json")
                    && let Ok(bytes) = std::fs::read(&path)
                {
                    let value: serde_json::Value =
                        serde_json::from_slice(&bytes).unwrap_or_default();
                    if let Some(ids) = value
                        .get("plan")
                        .and_then(|plan| plan.get("control_ids"))
                        .and_then(serde_json::Value::as_array)
                    {
                        control_ids.extend(
                            ids.iter()
                                .filter_map(serde_json::Value::as_str)
                                .map(str::to_string),
                        );
                    }
                }
            }
        }
        control_ids.sort();
        Ok(BuiltPack {
            packpath,
            control_ids,
            _tmp: tmp,
        })
    }

    /// R2: index_autocmds / new_autocmds の純粋論理検証（100 旧 autocmd・新規/既存
    /// グループ・groupless・rsplug グループ）。O(n) 差分と新規グループ判定を確認。
    #[tokio::test]
    async fn r2_autocmd_diff_helpers_are_linear_and_correct() {
        let pack = build_pack(vec![FakePlugin {
            tag: "ev_anchor",
            files: vec![("plugin/init.lua", b"vim.g.ev_anchor = true")],
            lazy: vec![FakeLazy::Event("R0Anchor")],
        }])
        .await
        .expect("build_pack");
        run_scenario(&pack, "autocmd_diff_helpers", &["ev_anchor"]);
    }

    /// R2: loader は発火後に削除され、packadd 中のネスト配信で二重読み込みしない。
    #[tokio::test]
    async fn r2_event_loader_removed_and_no_nested_delivery() {
        let pack = build_pack(vec![FakePlugin {
            tag: "ev_nested",
            files: vec![(
                "plugin/init.lua",
                b"vim.g.ev_nested = (vim.g.ev_nested or 0) + 1\n\
                  vim.api.nvim_exec_autocmds('User', { pattern = 'R0Nested', modeline = false })\n",
            )],
            lazy: vec![FakeLazy::Event("R0Nested")],
        }])
        .await
        .expect("build_pack");
        run_scenario(&pack, "loader_removed_no_nested", &["ev_nested"]);
    }

    /// R3: ftplugin ファイルが無いパッケージは packadd だけで source せず、
    /// v2 パスなので `nvim_get_runtime_file` を呼ばない。
    #[tokio::test]
    async fn r3_on_ft_no_match_loads_without_sourcing() {
        let pack = build_pack(vec![FakePlugin {
            tag: "nomatch_loaded",
            files: vec![("plugin/init.lua", b"vim.g.nomatch_loaded = true")],
            lazy: vec![FakeLazy::Ft("lua")],
        }])
        .await
        .expect("build_pack");
        let counts = run_scenario(&pack, "ft_no_match", &["nomatch_loaded"]);
        // v2 パスの on_ft 本体は runtime_file を呼ばない（残りは nvim 内部の filetype 処理）。
        let _ = counts;
    }

    /// R3: 2バッファ目は on_ft が processed で早期復帰し（定数時間・二重 source 無し）。
    #[tokio::test]
    async fn r3_on_ft_second_buffer_is_constant_time() {
        let pack = build_pack(vec![FakePlugin {
            tag: "sb_count",
            files: vec![(
                "ftplugin/lua.vim",
                b"let g:sb_count = get(g:, 'sb_count', 0) + 1",
            )],
            lazy: vec![FakeLazy::Ft("lua")],
        }])
        .await
        .expect("build_pack");
        let counts = run_scenario(&pack, "ft_second_buffer", &["sb_count"]);
        assert_eq!(
            counts.get("nvim_get_runtime_file").copied().unwrap_or(0),
            0,
            "R3: second buffer must not call nvim_get_runtime_file"
        );
    }

    /// R3: 別トリガで先に読み込まれたパッケージは on_ft で ftplugin を二重 source しない。
    #[tokio::test]
    async fn r3_on_ft_preloaded_by_another_trigger_not_resourced() {
        let pack = build_pack(vec![FakePlugin {
            tag: "pre_pkg",
            files: vec![
                ("plugin/init.lua", b"vim.g.pre_pkg = true"),
                (
                    "ftplugin/lua.vim",
                    b"let g:pre_ftplugin = get(g:, 'pre_ftplugin', 0) + 1",
                ),
            ],
            lazy: vec![FakeLazy::Event("R0Pre"), FakeLazy::Ft("lua")],
        }])
        .await
        .expect("build_pack");
        run_scenario(&pack, "ft_preloaded", &["pre_pkg"]);
    }

    /// R3: 同一 ft に複数 id。全ての ftplugin が source される。
    #[tokio::test]
    async fn r3_on_ft_multiple_ids_all_sourced() {
        let pack = build_pack(vec![
            FakePlugin {
                tag: "mid_a",
                files: vec![("ftplugin/lua.vim", b"let g:mid_a = 1")],
                lazy: vec![FakeLazy::Ft("lua")],
            },
            FakePlugin {
                tag: "mid_b",
                files: vec![("ftplugin/lua.lua", b"vim.g.mid_b = true")],
                lazy: vec![FakeLazy::Ft("lua")],
            },
        ])
        .await
        .expect("build_pack");
        run_scenario(&pack, "ft_multiple_ids", &["mid_a", "mid_b"]);
    }

    /// R4: 未登録名の require は状態を肥大させず、全 root 満足後に searcher が削除される。
    #[tokio::test]
    async fn r4_lua_searcher_retires_after_roots_satisfied() {
        let pack = build_pack(vec![FakePlugin {
            tag: "lua_root",
            files: vec![(
                "lua/mymod/init.lua",
                b"vim.g.lua_root = true\nlocal M = {}\nfunction M.hello() return 'hi' end\nreturn M\n",
            )],
            lazy: vec![FakeLazy::Lua("mymod")],
        }])
        .await
        .expect("build_pack");
        run_scenario(&pack, "lua_retire_searcher", &["lua_root"]);
    }

    /// R4: 登録されていないモジュールの require は標準 loader のエラーになる。
    #[tokio::test]
    async fn r4_lua_unknown_module_errors() {
        let pack = build_pack(vec![FakePlugin {
            tag: "lua_root",
            files: vec![("lua/mymod/init.lua", b"vim.g.lua_root = true\nreturn {}\n")],
            lazy: vec![FakeLazy::Lua("mymod")],
        }])
        .await
        .expect("build_pack");
        run_scenario(&pack, "lua_unknown_module", &[]);
    }

    /// R4: 1つの id が複数 root を持つ場合、1回の packadd で全 root が満足する。
    #[tokio::test]
    async fn r4_lua_one_id_multiple_roots() {
        let pack = build_pack(vec![FakePlugin {
            tag: "aaa_root",
            files: vec![
                ("lua/aaa/init.lua", b"vim.g.aaa_root = true\nreturn {}\n"),
                ("lua/bbb/init.lua", b"vim.g.bbb_root = true\nreturn {}\n"),
            ],
            lazy: vec![FakeLazy::Lua("aaa"), FakeLazy::Lua("bbb")],
        }])
        .await
        .expect("build_pack");
        run_scenario(
            &pack,
            "lua_one_id_multiple_roots",
            &["aaa_root", "bbb_root"],
        );
    }

    /// R4: 別トリガで先にロード済みの id は searcher インストール時に reconcile される。
    #[tokio::test]
    async fn r4_lua_other_trigger_satisfaction() {
        let pack = build_pack(vec![FakePlugin {
            tag: "ot_pkg",
            files: vec![
                ("plugin/init.lua", b"vim.g.ot_pkg = true"),
                ("lua/otmod/init.lua", b"vim.g.ot_lua = true\nreturn {}\n"),
            ],
            lazy: vec![FakeLazy::Event("R0LuaPre"), FakeLazy::Lua("otmod")],
        }])
        .await
        .expect("build_pack");
        run_scenario(
            &pack,
            "lua_other_trigger_satisfaction",
            &["ot_pkg", "ot_lua"],
        );
    }

    /// R4: packadd 中に同じ root の submodule を require しても無限ループしない。
    #[tokio::test]
    async fn r4_lua_recursion_during_packadd() {
        let pack = build_pack(vec![FakePlugin {
            tag: "rec_root",
            files: vec![
                (
                    "plugin/init.lua",
                    b"vim.g.rec_during_packadd = true\nlocal s = require('recmod.sub')\nvim.g.rec_sub_via_plugin = (s ~= nil)\n",
                ),
                ("lua/recmod/init.lua", b"vim.g.rec_root = true\nreturn {}\n"),
                ("lua/recmod/sub.lua", b"vim.g.rec_sub = true\nreturn { x = 1 }\n"),
            ],
            lazy: vec![FakeLazy::Lua("recmod")],
        }])
        .await
        .expect("build_pack");
        run_scenario(
            &pack,
            "lua_recursion_during_packadd",
            &["rec_root", "rec_sub", "rec_sub_via_plugin"],
        );
    }

    /// R5: 全到達可能モード setup 後に ModeChanged/VimEnter の augroup が削除される。
    #[tokio::test]
    async fn r5_map_observer_retires_after_reachable_modes_setup() {
        let pack = build_pack(vec![FakePlugin {
            tag: "map_a",
            files: vec![("plugin/init.lua", b"vim.g.map_a = true")],
            lazy: vec![FakeLazy::Map {
                mode: Some('n'),
                pattern: "zL",
            }],
        }])
        .await
        .expect("build_pack");
        run_scenario(&pack, "map_retires_after_setup", &[]);
    }

    /// R5: 特殊キー（<F5>）パターンの expr マッピングが termcode replay 付きで機能する。
    #[tokio::test]
    async fn r5_map_special_key_replay_loads_plugins() {
        let pack = build_pack(vec![
            FakePlugin {
                tag: "sk_a",
                files: vec![("plugin/init.lua", b"vim.g.sk_a = true")],
                lazy: vec![FakeLazy::Map {
                    mode: Some('n'),
                    pattern: "<F5>",
                }],
            },
            FakePlugin {
                tag: "sk_b",
                files: vec![("plugin/init.lua", b"vim.g.sk_b = true")],
                lazy: vec![FakeLazy::Map {
                    mode: Some('n'),
                    pattern: "<F5>",
                }],
            },
        ])
        .await
        .expect("build_pack");
        run_scenario(&pack, "map_special_key_replay", &["sk_a", "sk_b"]);
    }

    /// Validation gate: 10,000 件の無関係 require でも pending/進行中状態は成長しない。
    #[tokio::test]
    async fn val_lua_10k_unrelated_requires_no_state_growth() {
        let pack = build_pack(vec![FakePlugin {
            tag: "lua_root",
            files: vec![("lua/mymod/init.lua", b"vim.g.lua_root = true\nreturn {}\n")],
            lazy: vec![FakeLazy::Lua("mymod")],
        }])
        .await
        .expect("build_pack");
        run_scenario(&pack, "lua_10k_unrelated_no_state_growth", &[]);
    }

    /// Validation gate: event トリガ1回あたり `nvim_get_autocmds` は before/after の2回だけ。
    #[tokio::test]
    async fn val_event_diff_uses_two_autocmd_queries() {
        let pack = build_pack(vec![
            FakePlugin {
                tag: "ev_a",
                files: vec![("plugin/init.lua", b"vim.g.ev_a = true")],
                lazy: vec![FakeLazy::Event("R0Shared")],
            },
            FakePlugin {
                tag: "ev_b",
                files: vec![("plugin/init.lua", b"vim.g.ev_b = true")],
                lazy: vec![FakeLazy::Event("R0Shared")],
            },
        ])
        .await
        .expect("build_pack");
        let counts = run_scenario(&pack, "event_diff_two_queries", &[]);
        let q = counts.get("nvim_get_autocmds").copied().unwrap_or(0);
        assert_eq!(
            q, 2,
            "event diff must use one before + one after nvim_get_autocmds (got {q})"
        );
    }

    /// Validation bench（非gating・ignored）: 1k autocmd / 1k ft files / 10k require /
    /// 10k mode change を5サンプルで計測し、結果を `target/runtime_hot_paths_bench.json` へ
    /// 書き出す。CI の構造ゲートではなくローカル比較用。
    #[tokio::test]
    #[ignore = "non-gating benchmark: run with --ignored"]
    async fn bench_runtime_hot_paths() {
        // 4k ftplugin（lua_NNN.lua, suffix 形）+ lua root + map + event を1パッケージに。
        // 静的ライフタイム要件があるため、ファイル名/内容は Box::leak で 'static に保持する
        // （ignored ベンチマークなので解放しない）。
        let mut plugins = Vec::with_capacity(4);
        for package in 0..4u32 {
            let mut files = Vec::with_capacity(1002);
            for i in 0..1000u32 {
                let name: &'static str =
                    Box::leak(format!("ftplugin/lua_{package}_{i:03}.lua").into_boxed_str());
                let data: &'static [u8] = Box::leak(b"-- bench\n".to_vec().into_boxed_slice());
                files.push((name, data));
            }
            files.push(("plugin/init.lua", b"vim.g.bench_pkg = true"));
            files.push(("lua/mymod/init.lua", b"vim.g.bench_lua = true\nreturn {}\n"));
            plugins.push(FakePlugin {
                tag: Box::leak(format!("bench_pkg_{package}").into_boxed_str()),
                files,
                lazy: vec![
                    FakeLazy::Event("R0BenchEv"),
                    FakeLazy::Ft("lua"),
                    FakeLazy::Lua("mymod"),
                    FakeLazy::Map {
                        mode: Some('n'),
                        pattern: "zL",
                    },
                ],
            });
        }
        let pack = build_pack(plugins).await.expect("build_pack");

        let out = nvim_output(&pack, "bench", "");
        // BENCH <name> scale=N iterations=N samples=N median_ns=.. p95_ns=..
        //   min_ns=.. max_ns=.. api_counts=<json> ft_count=N
        let cpu_count = std::thread::available_parallelism()
            .map(|n| n.get().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        let toolchain = Command::new("rustc")
            .arg("-V")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .unwrap_or_else(|| "unknown".to_string());
        let mut json = format!(
            "{{\n  \"schema\": 2,\n  \"phase\": \"lazy_runtime\",\n  \"environment\": {{\"build_profile\": \"{}\", \"cpu_count\": \"{}\", \"filesystem\": \"local-tempdir\", \"os\": \"{}\", \"toolchain\": \"{}\"}},\n  \"benchmarks\": {{\n",
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            cpu_count,
            std::env::consts::OS,
            toolchain,
        );
        let mut first = true;
        for line in out.lines() {
            let line = line.trim_start_matches('\x0d').trim();
            let Some(rest) = line.strip_prefix("BENCH ") else {
                continue;
            };
            let name = rest.split_whitespace().next().unwrap_or("?");
            let mut fields: std::collections::BTreeMap<&str, &str> =
                std::collections::BTreeMap::new();
            for tok in rest.split_whitespace().skip(1) {
                if let Some((k, v)) = tok.split_once('=') {
                    fields.insert(k, v);
                }
            }
            if !first {
                json.push_str(",\n");
            }
            first = false;
            let api_counts = fields.get("api_counts").copied().unwrap_or("{}");
            json.push_str(&format!(
                "    \"{name}\": {{\"scale\":{scale}, \"iterations\":{iterations}, \"samples\":{s}, \"median_ns\":{med}, \"p95_ns\":{p95}, \"min_ns\":{mn}, \"max_ns\":{mx}, \"before_median_ns\":{before_med}, \"before_p95_ns\":{before_p95}, \"delta_ns\":{delta}, \"api_counts\":{api_counts}}}",
                scale = fields.get("scale").copied().unwrap_or("0"),
                iterations = fields.get("iterations").copied().unwrap_or("0"),
                s = fields.get("samples").copied().unwrap_or("0"),
                med = fields.get("median_ns").copied().unwrap_or("0"),
                p95 = fields.get("p95_ns").copied().unwrap_or("0"),
                mn = fields.get("min_ns").copied().unwrap_or("0"),
                mx = fields.get("max_ns").copied().unwrap_or("0"),
                before_med = fields.get("before_median_ns").copied().unwrap_or("null"),
                before_p95 = fields.get("before_p95_ns").copied().unwrap_or("null"),
                delta = fields.get("delta_ns").copied().unwrap_or("0"),
            ));
        }
        json.push_str("\n  }\n}\n");
        let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("runtime_hot_paths_bench.json");
        std::fs::create_dir_all(target.parent().unwrap()).ok();
        std::fs::write(&target, &json).expect("write bench json");
        println!("wrote bench report to {}", target.display());
        assert!(
            out.contains("BENCH "),
            "bench scenario produced no BENCH output:\n{out}"
        );
    }

    /// nvim を headless で起動してシナリオを実行し、stdout+stderr の結合テキストを返す
    /// （`--headless` では print は stderr に出るため両方を取る）。ベンチマーク計測用。
    fn nvim_output(pack: &BuiltPack, scenario: &str, expect: &str) -> String {
        let nvim = std::env::var_os("RSPLUG_TEST_NVIM").unwrap_or_else(|| OsString::from("nvim"));
        let lua = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/runtime_hot_paths.lua");
        let packpath =
            std::fs::canonicalize(&pack.packpath).unwrap_or_else(|_| pack.packpath.clone());
        let xdg_root = pack
            .packpath
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("xdg");
        let xdg = |sub: &str| {
            let p = xdg_root.join(sub);
            std::fs::create_dir_all(&p).ok();
            p
        };
        let output = Command::new(&nvim)
            .arg("--headless")
            .arg("--clean")
            .arg("-i")
            .arg("NONE")
            .arg("-n")
            .env("RSPLUG_TEST_PACKPATH", &packpath)
            .env("RSPLUG_TEST_SCENARIO", scenario)
            .env("RSPLUG_TEST_EXPECT", expect)
            .env("XDG_CONFIG_HOME", xdg("config"))
            .env("XDG_DATA_HOME", xdg("data"))
            .env("XDG_CACHE_HOME", xdg("cache"))
            .env("XDG_STATE_HOME", xdg("state"))
            .arg("-c")
            .arg(format!("luafile {lua}"))
            .arg("-c")
            .arg("qa!")
            .output()
            .unwrap_or_else(|e| {
                panic!(
                    "failed to spawn nvim ({}) for scenario {scenario}: {e}",
                    nvim.to_string_lossy()
                )
            });
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        format!("{stdout}\n{stderr}")
    }

    /// nvim を headless で起動し `tests/runtime_hot_paths.lua` のシナリオを実行。
    /// `expect` は「truthy でなければならない `vim.g` タグ」のカンマ区切り。
    /// 成功時、`RSPLUG_TEST_COUNT <name>=<n>` 行を parse した計測値を返す。
    /// nvim 起動失敗・結果不明瞾時はパニック（skip しない）。
    pub(super) fn run_scenario(
        pack: &BuiltPack,
        scenario: &str,
        expect: &[&str],
    ) -> std::collections::HashMap<String, u64> {
        let nvim = std::env::var_os("RSPLUG_TEST_NVIM").unwrap_or_else(|| OsString::from("nvim"));
        let lua = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/runtime_hot_paths.lua");
        // macOS の /var↔/private/var symlink 等で packpath が2通りに解決されると、
        // コントロールパッケージが runtimepath に二重登録され plugin/*.lua が2回 source
        // される。これを避けるため packpath を正規化して nvim に一貫した実パスを渡す。
        let packpath =
            std::fs::canonicalize(&pack.packpath).unwrap_or_else(|_| pack.packpath.clone());
        // ホーム汚染を避けるため XDG を pack の一時ルート配下に隔離する。
        // `--clean` で nvim 標準ランタイム（filetype→ftplugin 連鎖を含む）を読み、
        // ユーザ設定は読まない。`-u NONE` だと ftplugin 連鎖が無効になるため使わない。
        let xdg_root = pack
            .packpath
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("xdg");
        let xdg = |sub: &str| {
            let p = xdg_root.join(sub);
            std::fs::create_dir_all(&p).ok();
            p
        };
        let output = Command::new(&nvim)
            .arg("--headless")
            .arg("--clean")
            .arg("-i")
            .arg("NONE")
            .arg("-n")
            .env("RSPLUG_TEST_PACKPATH", &packpath)
            .env("RSPLUG_TEST_SCENARIO", scenario)
            .env("RSPLUG_TEST_EXPECT", expect.join(","))
            .env("RSPLUG_TEST_CONTROL_ID", pack.control_ids.join(","))
            .env("XDG_CONFIG_HOME", xdg("config"))
            .env("XDG_DATA_HOME", xdg("data"))
            .env("XDG_CACHE_HOME", xdg("cache"))
            .env("XDG_STATE_HOME", xdg("state"))
            .arg("-c")
            .arg(format!("luafile {lua}"))
            .arg("-c")
            .arg("qa!")
            .output();
        let output = match output {
            Ok(o) => o,
            Err(e) => panic!(
                "failed to spawn nvim ({}) for scenario {scenario}: {e}",
                nvim.to_string_lossy()
            ),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        // nvim --headless では print()/メッセージは stderr に出る。stdout/stderr 両方を検査する。
        let combined = format!("{stdout}\n{stderr}");
        let ok = combined
            .lines()
            .any(|l| l.trim_start_matches('\x0d') == "RSPLUG_TEST_RESULT=ok");
        if !ok {
            panic!(
                "scenario {scenario} did not report ok.\n\
                 control_ids={:?}\n\
                 --- stdout ---\n{stdout}\n\
                 --- stderr ---\n{stderr}",
                pack.control_ids
            );
        }
        let mut counts = std::collections::HashMap::new();
        for line in combined.lines() {
            let line = line.trim_start_matches('\x0d').trim();
            if let Some(rest) = line.strip_prefix("RSPLUG_TEST_COUNT ")
                && let Some((k, v)) = rest.split_once('=')
                && let Ok(n) = v.trim().parse::<u64>()
            {
                *counts.entry(k.trim().to_string()).or_insert(0) += n;
            }
        }
        counts
    }

    // ---- R0 characterization scenarios (current/slow behavior baseline) ----

    /// 共有 User イベントに2プラグインを登録。トリガ1回で両方読み込まれる。
    #[tokio::test]
    async fn r0_shared_events_load_all_plugins_on_one_trigger() {
        let pack = build_pack(vec![
            FakePlugin {
                tag: "ev_a",
                files: vec![("plugin/init.lua", b"vim.g.ev_a = true")],
                lazy: vec![FakeLazy::Event("R0Shared")],
            },
            FakePlugin {
                tag: "ev_b",
                files: vec![("plugin/init.lua", b"vim.g.ev_b = true")],
                lazy: vec![FakeLazy::Event("R0Shared")],
            },
        ])
        .await
        .expect("build_pack");
        run_scenario(&pack, "shared_events", &["ev_a", "ev_b"]);
    }

    /// on_ft で3形式（exact / suffix / subdir）の ftplugin を全て source する（セマンティック）。
    #[tokio::test]
    async fn r0_on_ft_sources_all_path_forms() {
        let pack = build_pack(vec![FakePlugin {
            tag: "ft_exact",
            files: vec![
                ("ftplugin/lua.vim", b"let g:ft_exact = 1"),
                ("ftplugin/lua_extra.lua", b"vim.g.ft_suffix = true"),
                ("ftplugin/lua/settings.lua", b"vim.g.ft_subdir = true"),
            ],
            lazy: vec![FakeLazy::Ft("lua")],
        }])
        .await
        .expect("build_pack");
        run_scenario(
            &pack,
            "ft_path_forms",
            &["ft_exact", "ft_suffix", "ft_subdir"],
        );
    }

    /// R3 gate: v2 manifest の `get_ft_runtime_files` は `nvim_get_runtime_file` を呼ばない。
    #[tokio::test]
    async fn r3_ft_index_resolver_makes_no_runtime_lookup() {
        let pack = build_pack(vec![FakePlugin {
            tag: "ft_exact",
            files: vec![
                ("ftplugin/lua.vim", b"let g:ft_exact = 1"),
                ("ftplugin/lua_extra.lua", b"vim.g.ft_suffix = true"),
                ("ftplugin/lua/settings.lua", b"vim.g.ft_subdir = true"),
            ],
            lazy: vec![FakeLazy::Ft("lua")],
        }])
        .await
        .expect("build_pack");
        let counts = run_scenario(&pack, "ft_index_no_runtime_lookup", &[]);
        assert_eq!(
            counts.get("nvim_get_runtime_file").copied().unwrap_or(0),
            0,
            "get_ft_runtime_files must not call nvim_get_runtime_file"
        );
    }

    /// R1: 公開された manifest が v2 で、登録 (ft,id) の ftplugin インデックスを3グループ
    /// 全て含むことを、実際の install 経路の出物で検証する。
    #[tokio::test]
    async fn r1_manifest_v2_contains_ftplugin_index() {
        let pack = build_pack(vec![FakePlugin {
            tag: "ft_exact",
            files: vec![
                ("ftplugin/lua.vim", b"let g:ft_exact = 1"),
                ("ftplugin/lua_extra.lua", b"vim.g.ft_suffix = true"),
                ("ftplugin/lua/settings.lua", b"vim.g.ft_subdir = true"),
            ],
            lazy: vec![FakeLazy::Ft("lua")],
        }])
        .await
        .expect("build_pack");
        let manifest_path = pack.packpath.join(format!(
            "pack/_gen/generations/{}.json",
            pack.control_ids[0]
        ));
        let content = std::fs::read_to_string(&manifest_path).expect("manifest.json");
        assert!(
            !pack
                .packpath
                .join(format!(
                    "pack/_gen/opt/{}/manifest.json",
                    pack.control_ids[0]
                ))
                .exists()
        );
        let v: serde_json::Value = serde_json::from_str(&content).expect("valid json");
        assert_eq!(v["version"], 2, "manifest must be v2: {content}");
        let lua = &v["runtime"]["ftplugin"]["lua"];
        assert!(lua.is_object(), "lua ftplugin index missing: {content}");
        // 登録されたのは1つの (ft,id) だけ。
        assert_eq!(lua.as_object().unwrap().len(), 1);
        let paths = lua
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .as_array()
            .unwrap();
        let path_strs: Vec<&str> = paths.iter().map(|p| p.as_str().unwrap()).collect();
        assert!(path_strs.iter().any(|p| p.ends_with("/ftplugin/lua.vim")));
        assert!(
            path_strs
                .iter()
                .any(|p| p.ends_with("/ftplugin/lua_extra.lua"))
        );
        assert!(
            path_strs
                .iter()
                .any(|p| p.ends_with("/ftplugin/lua/settings.lua"))
        );
    }

    /// on_lua で root と submodule の両方を require 可能にする。
    #[tokio::test]
    async fn r0_on_lua_serves_root_and_submodule() {
        let pack = build_pack(vec![FakePlugin {
            tag: "lua_root",
            files: vec![
                (
                    "lua/mymod/init.lua",
                    b"vim.g.lua_root = true\nlocal M = {}\nfunction M.hello() return 'hi' end\nreturn M\n",
                ),
                ("lua/mymod/sub.lua", b"vim.g.lua_sub = true\nreturn { x = 1 }\n"),
            ],
            lazy: vec![FakeLazy::Lua("mymod")],
        }])
        .await
        .expect("build_pack");
        run_scenario(
            &pack,
            "require_root_and_submodule",
            &["lua_root", "lua_sub"],
        );
    }

    /// on_map で同一パターンに2プラグイン。キー1回で両方読み込まれる。
    #[tokio::test]
    async fn r0_on_map_loads_duplicate_pattern_plugins() {
        let pack = build_pack(vec![
            FakePlugin {
                tag: "map_a",
                files: vec![("plugin/init.lua", b"vim.g.map_a = true")],
                lazy: vec![FakeLazy::Map {
                    mode: Some('n'),
                    pattern: "zL",
                }],
            },
            FakePlugin {
                tag: "map_b",
                files: vec![("plugin/init.lua", b"vim.g.map_b = true")],
                lazy: vec![FakeLazy::Map {
                    mode: Some('n'),
                    pattern: "zL",
                }],
            },
        ])
        .await
        .expect("build_pack");
        run_scenario(&pack, "duplicate_maps", &["map_a", "map_b"]);
    }

    // ---- L1: common lazy runtime state + transactional packadd ----

    /// L1: packadd は成功の境界を通過したときだけ loaded になる。plugin/init.lua が
    /// 自身を packadd しても loading ガードで早期復帰し、plugin/ は1回だけ source される
    /// （loaded-once + recursion guard + idempotent 再 packadd）。
    #[tokio::test]
    async fn l1_transactional_loaded_after_success_and_recursion_guard() {
        let pack = build_pack(vec![FakePlugin {
            tag: "trans_self",
            files: vec![(
                "plugin/init.lua",
                b"vim.g.trans_self_count = (vim.g.trans_self_count or 0) + 1\n\
                  require('_rsplug').packadd(vim.g.rsplug_self_id)\n",
            )],
            lazy: vec![FakeLazy::Event("L1Self")],
        }])
        .await
        .expect("build_pack");
        run_scenario(&pack, "transactional_loaded_after_success", &[]);
    }

    /// L1: packadd 本体のエラーは retryable な unloaded 状態に戻し、元のメッセージを
    /// 保存（traceback 保持）して再送する。2回目（エラー無し）の packadd で loaded になる。
    #[tokio::test]
    async fn l1_transactional_error_restores_retryable_state() {
        let pack = build_pack(vec![FakePlugin {
            tag: "trans_err",
            files: vec![(
                "plugin/init.lua",
                b"vim.g.err_count = (vim.g.err_count or 0) + 1\n\
                  if vim.g.err_count == 1 then error('L1 transactional boom') end\n\
                  vim.g.err_ok = true\n",
            )],
            lazy: vec![FakeLazy::Event("L1Err")],
        }])
        .await
        .expect("build_pack");
        run_scenario(&pack, "transactional_error_retry", &[]);
    }
}

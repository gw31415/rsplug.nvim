use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, btree_map::Keys},
    fmt::Display,
    iter::Sum,
    ops::AddAssign,
    path::PathBuf,
    sync::Arc,
};

use sailfish::{TemplateSimple, runtime::Render};

use super::*;
use crate::rsplug::util::hash;

/// docファイルを束ねたヘルプ専用プラグインの source_name。
/// `pack/_gen/start/` への配置判定に使われる。
pub(super) const DOC_PLUGIN_NAME: &str = "_rsplug:doc";

struct PkgId2ScriptsItem {
    pkgid: PluginIDStr,
    script: SetupScript,
    order: usize,
    start: bool, // もし読み込みプラグイン元が LazyType::Start なら、他のスクリプトと別の仕組みでスクリプトを呼び出す必要があるため
}

#[derive(Hash, Eq, PartialEq, PartialOrd, Ord)]
enum AfterOrBefore {
    After,
    Before,
}

impl AfterOrBefore {
    fn as_str(&self) -> &'static str {
        match self {
            AfterOrBefore::After => "lua_after",
            AfterOrBefore::Before => "lua_before",
        }
    }
}

impl Display for AfterOrBefore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Render for AfterOrBefore {
    fn render(&self, b: &mut sailfish::runtime::Buffer) -> Result<(), sailfish::RenderError> {
        b.push_str(self.as_str());
        Ok(())
    }
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
                    let lua_after = lua_after.into_iter().map(|s| (AfterOrBefore::After, s));
                    let lua_before = lua_before.into_iter().map(|s| (AfterOrBefore::Before, s));
                    let mut script_set: BTreeMap<AfterOrBefore, Vec<String>> = Default::default();
                    for (script_type, content) in lua_after.chain(lua_before) {
                        let module_id =
                            format!("{script_type}_{}", hash::digest_hash_hex_string(&content));
                        plugs.push(instant_startup_pkg(
                            &format!("lua/{module_id}.lua"),
                            content.into_bytes(),
                        ));
                        script_set.entry(script_type).or_default().push(module_id);
                    }
                    for content in lua_start {
                        scripts_startup.push((order, pkgid.clone(), content));
                    }

                    if start {
                        scripts_start.push((order, pkgid.clone()));
                    }
                    if !script_set.is_empty() {
                        scripts_lazy.push((pkgid, script_set));
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
            let on_lua: Cow<'static, [u8]> = OnLuaTemplate {
                luam2pkgid: &luam2pkgid,
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
            let data = include_bytes!("../../../templates/plugin/on_map.lua");
            plugs.push(instant_startup_pkg(
                &format!("plugin/{}.lua", hash::digest_hash_hex_string(data)),
                data,
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
    pkgid2scripts: Vec<(PluginIDStr, BTreeMap<AfterOrBefore, Vec<String>>)>,
    startup_plugins: Vec<PluginIDStr>,
    source2pkgid: Vec<(PluginIDStr, Vec<PluginIDStr>)>,
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
}

#[derive(TemplateSimple)]
#[template(path = "lua/_rsplug/on_map/mode__.stpl")]
#[template(escape = false)]
struct OnMapTemplate<'a> {
    mode: &'a ModeChar,
    keypattern2pkgid: &'a BTreeMap<ModeChar, BTreeMap<Arc<String>, Vec<PluginIDStr>>>,
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(rendered.contains("autoload_handler('foo#bar')"));
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
    fn custom_packadd_template_packadds_startup_plugins() {
        let startup_plugin = b"startup-plugin".plugin_id().as_str();
        let rendered = CustomPackaddTemplate {
            pkgid2scripts: Vec::new(),
            startup_plugins: vec![startup_plugin.clone()],
            source2pkgid: Vec::new(),
        }
        .render_once()
        .unwrap();

        assert!(rendered.contains(&format!("local startup_plugins = {{'{startup_plugin}',}}")));
        assert!(!rendered.contains("startup_scripts"));
        assert!(rendered.contains("require '_rsplug'.packadd(id, true)"));
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

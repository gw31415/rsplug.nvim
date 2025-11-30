use std::{
    borrow::Cow,
    collections::{BTreeMap, btree_map::Keys},
    fmt::Display,
    iter::Sum,
    ops::AddAssign,
    path::PathBuf,
    sync::Arc,
};

use hashbrown::HashMap;
use sailfish::{TemplateSimple, runtime::Render};

use super::*;
use crate::rsplug::util::hash;

struct PkgId2ScriptsItem {
    pkgid: PluginIDStr,
    script: SetupScript,
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
pub struct PlugCtl {
    pkgid2scripts: Vec<PkgId2ScriptsItem>,
    event2pkgid: BTreeMap<Autocmd, Vec<PluginIDStr>>,
    cmd2pkgid: BTreeMap<UserCmd, Vec<PluginIDStr>>,
    ft2pkgid: BTreeMap<FileType, Vec<PluginIDStr>>,
    luam2pkgid: BTreeMap<LuaModule, Vec<PluginIDStr>>,
    keypattern2pkgid: BTreeMap<ModeChar, BTreeMap<Arc<String>, Vec<PluginIDStr>>>,
    overwrite_files: BTreeMap<PluginID, HowToPlaceFiles>,
}

/// 単スクリプトをランタイムパスに配置するためのパッケージを作成する。
fn instant_startup_pkg(path: &str, data: impl Into<Cow<'static, [u8]>>) -> LoadedPlugin {
    let data = data.into();
    let id = PluginID::new(&data) + PluginID::new(path);
    let files = HashMap::from([(
        PathBuf::from(path),
        FileItem {
            source: Arc::new(FileSource::File { data }),
            merge_type: MergeType::Overwrite,
        },
    )]);
    LoadedPlugin {
        id,
        lazy_type: LazyType::Start,
        files: HowToPlaceFiles::CopyEachFile(files),
        script: Default::default(),
        is_plugctl: true,
    }
}

impl From<PlugCtl> for Vec<LoadedPlugin> {
    fn from(value: PlugCtl) -> Vec<LoadedPlugin> {
        if value.is_empty() {
            return Vec::with_capacity(0);
        }
        let PlugCtl {
            pkgid2scripts,
            event2pkgid,
            cmd2pkgid,
            ft2pkgid,
            luam2pkgid,
            keypattern2pkgid,
            overwrite_files,
        } = value;

        let mut plugs = Vec::new();

        {
            // Add packages to place scripts that does the initial setup of the plugin
            let (pkgid2scripts, startplugins_setupscripts) = pkgid2scripts.into_iter().fold(
                (Vec::new(), BTreeMap::new()),
                |(mut scripts_lazy, mut scripts_start),
                 PkgId2ScriptsItem {
                     pkgid,
                     script,
                     start,
                 }| {
                    let SetupScript {
                        lua_after,
                        lua_before,
                    } = script;
                    let lua_after = lua_after.into_iter().map(|s| (AfterOrBefore::After, s));
                    let lua_before = lua_before.into_iter().map(|s| (AfterOrBefore::Before, s));
                    let mut script_set: BTreeMap<AfterOrBefore, Vec<String>> = Default::default();
                    {
                        let script_set = if start {
                            &mut scripts_start
                        } else {
                            &mut script_set
                        };
                        for (script_type, content) in lua_after.chain(lua_before) {
                            let module_id = format!(
                                "{script_type}_{}",
                                hash::digest_hex_string(content.as_bytes())
                            );
                            plugs.push(instant_startup_pkg(
                                &format!("lua/{module_id}.lua"),
                                content.into_bytes(),
                            ));
                            script_set.entry(script_type).or_default().push(module_id);
                        }
                    }

                    if !script_set.is_empty() {
                        scripts_lazy.push((pkgid, script_set));
                    }
                    (scripts_lazy, scripts_start)
                },
            );
            plugs.push(instant_startup_pkg(
                "lua/_rsplug/init.lua",
                CustomPackaddTemplate { pkgid2scripts }
                    .render_once()
                    .unwrap()
                    .into_bytes(),
            ));
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
                path.push_str(&hash::digest_hex_string(&data));
                path.push_str(".lua");

                plugs.push(instant_startup_pkg(&path, data));
            }
        }

        if !event2pkgid.is_empty() {
            // on_event setup
            let events = event2pkgid.keys();
            let on_event_setup = OnEventSetupTemplate { events }
                .render_once()
                .unwrap()
                .into_bytes()
                .into();
            let on_event_setup_id = PluginID::new(&on_event_setup);
            let on_event = OnEventTemplate {
                event2pkgid: &event2pkgid,
            }
            .render_once()
            .unwrap()
            .into_bytes()
            .into();
            let on_event_id = PluginID::new(&on_event);
            let files = HashMap::from([
                (
                    PathBuf::from("lua/_rsplug/on_event.lua"),
                    FileItem {
                        source: Arc::new(FileSource::File { data: on_event }),
                        merge_type: MergeType::Overwrite,
                    },
                ),
                (
                    PathBuf::from(format!("plugin/{}.lua", on_event_setup_id.as_str())),
                    FileItem {
                        source: Arc::new(FileSource::File {
                            data: on_event_setup,
                        }),
                        merge_type: MergeType::Overwrite,
                    },
                ),
            ]);
            plugs.push({
                LoadedPlugin {
                    id: on_event_setup_id + on_event_id,
                    lazy_type: LazyType::Start,
                    files: HowToPlaceFiles::CopyEachFile(files),
                    script: Default::default(),
                    is_plugctl: true,
                }
            });
        }

        if !cmd2pkgid.is_empty() {
            // on_cmd setup
            plugs.push({
                let cmds = cmd2pkgid.keys();
                let on_cmd_setup = OnCmdSetupTemplate { cmds }
                    .render_once()
                    .unwrap()
                    .into_bytes()
                    .into();
                let on_cmd_setup_id = PluginID::new(&on_cmd_setup);
                let on_cmd = OnCmdTemplate {
                    cmd2pkgid: &cmd2pkgid,
                }
                .render_once()
                .unwrap()
                .into_bytes()
                .into();
                let on_cmd_id = PluginID::new(&on_cmd);
                let files = HashMap::from([
                    (
                        PathBuf::from("lua/_rsplug/on_cmd.lua"),
                        FileItem {
                            source: Arc::new(FileSource::File { data: on_cmd }),
                            merge_type: MergeType::Overwrite,
                        },
                    ),
                    (
                        PathBuf::from(format!("plugin/{}.lua", on_cmd_setup_id.as_str())),
                        FileItem {
                            source: Arc::new(FileSource::File { data: on_cmd_setup }),
                            merge_type: MergeType::Overwrite,
                        },
                    ),
                ]);
                LoadedPlugin {
                    id: on_cmd_id + on_cmd_setup_id,
                    lazy_type: LazyType::Start,
                    files: HowToPlaceFiles::CopyEachFile(files),
                    script: Default::default(),
                    is_plugctl: true,
                }
            });
        }
        if !luam2pkgid.is_empty() {
            let plugin_on_lua = include_bytes!("../../../templates/plugin/on_lua.lua");
            let plugin_on_lua_id = PluginID::new(plugin_on_lua);
            let on_lua = OnLuaTemplate {
                luam2pkgid: &luam2pkgid,
            }
            .render_once()
            .unwrap()
            .into_bytes()
            .into();
            let on_lua_id = PluginID::new(&on_lua);
            let files = HashMap::from([
                (
                    PathBuf::from("lua/_rsplug/on_lua.lua"),
                    FileItem {
                        source: Arc::new(FileSource::File { data: on_lua }),
                        merge_type: MergeType::Overwrite,
                    },
                ),
                (
                    PathBuf::from(format!("plugin/{}.lua", plugin_on_lua_id.as_str())),
                    FileItem {
                        source: Arc::new(FileSource::File {
                            data: plugin_on_lua.into(),
                        }),
                        merge_type: MergeType::Overwrite,
                    },
                ),
            ]);
            plugs.push(LoadedPlugin {
                id: plugin_on_lua_id + on_lua_id,
                lazy_type: LazyType::Start,
                files: HowToPlaceFiles::CopyEachFile(files),
                script: Default::default(),
                is_plugctl: true,
            });
        }
        if !keypattern2pkgid.is_empty() {
            let data = include_bytes!("../../../templates/plugin/on_map.lua");
            plugs.push(instant_startup_pkg(
                &format!("plugin/{}.lua", hash::digest_hex_string(data)),
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

        // Processing overwrite_files
        {
            let mut overwrite_copies_id: PluginID = PluginID::new(b"doc");
            let mut overwrite_copies = HashMap::new();
            for (id, files) in overwrite_files {
                match files {
                    HowToPlaceFiles::CopyEachFile(files) => {
                        // If CopyEachFile then merge
                        overwrite_copies_id += id;
                        overwrite_copies.extend(files);
                    }
                    HowToPlaceFiles::SymlinkDirectory(_) => {
                        panic!("SymlinkDirectory is not supported for overwrite_files in PlugCtl");
                    }
                }
            }
            if !overwrite_copies.is_empty() {
                plugs.push(LoadedPlugin {
                    id: overwrite_copies_id,
                    lazy_type: LazyType::Start,
                    files: HowToPlaceFiles::CopyEachFile(overwrite_copies),
                    script: Default::default(),
                    is_plugctl: true,
                });
            }
        }

        plugs
    }
}

impl AddAssign for PlugCtl {
    fn add_assign(&mut self, other: Self) {
        let Self {
            pkgid2scripts,
            event2pkgid,
            cmd2pkgid,
            ft2pkgid,
            luam2pkgid,
            keypattern2pkgid,
            overwrite_files,
        } = other;
        for (event, ids) in event2pkgid {
            self.event2pkgid
                .entry(event)
                .or_default()
                .extend(ids.into_iter());
        }
        self.pkgid2scripts.extend(pkgid2scripts);
        for (cmd, ids) in cmd2pkgid {
            self.cmd2pkgid
                .entry(cmd)
                .or_default()
                .extend(ids.into_iter());
        }
        for (ft, ids) in ft2pkgid {
            self.ft2pkgid.entry(ft).or_default().extend(ids.into_iter());
        }
        for (luam, ids) in luam2pkgid {
            self.luam2pkgid
                .entry(luam)
                .or_default()
                .extend(ids.into_iter());
        }
        for (key, pattern) in keypattern2pkgid {
            for (pattern, ids) in pattern {
                self.keypattern2pkgid
                    .entry(key.clone())
                    .or_default()
                    .entry(pattern)
                    .or_default()
                    .extend(ids.into_iter());
            }
        }
        self.overwrite_files.extend(overwrite_files);
    }
}

impl Sum for PlugCtl {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        let mut res = PlugCtl::new();
        for l in iter {
            res += l
        }
        res
    }
}

impl PlugCtl {
    /// Create empty
    pub fn new() -> Self {
        Default::default()
    }
    /// PlugCtlが空かどうか
    pub fn is_empty(&self) -> bool {
        let Self {
            pkgid2scripts: scripts,
            event2pkgid,
            cmd2pkgid,
            ft2pkgid,
            luam2pkgid,
            keypattern2pkgid,
            overwrite_files,
        } = self;
        event2pkgid.is_empty()
            && scripts.is_empty()
            && cmd2pkgid.is_empty()
            && ft2pkgid.is_empty()
            && luam2pkgid.is_empty()
            && keypattern2pkgid.values().all(|v| v.is_empty())
            && overwrite_files.is_empty()
    }

    /// パッケージ情報を読み込み、 PlugCtl を作成する。
    /// 読み込む情報が要らない場合は `None` を返す。
    /// NOTE: Package はインストールされる必要があるため、変更を抑制する意図で PackageID の所有権を奪う。
    /// その他必要な情報のみ引数に取る。
    pub(super) fn create(
        id: PluginID,
        lazy_type: LazyType,
        script: SetupScript,
        files: &mut HowToPlaceFiles,
    ) -> Self {
        // Steal `doc/**` files
        let mut overwrite_files = move |id: PluginID| {
            [(
                id,
                match files {
                    HowToPlaceFiles::CopyEachFile(map) => HowToPlaceFiles::CopyEachFile(
                        map.extract_if(|path, file| {
                            if path.starts_with("doc/") {
                                file.merge_type = MergeType::Overwrite;
                                true
                            } else {
                                false
                            }
                        })
                        .collect(),
                    ),
                    HowToPlaceFiles::SymlinkDirectory(_path) => {
                        // TODO: Copy doc files from symlinked directory
                        // Copy each _path.join("doc/*") file/dirs
                        HowToPlaceFiles::CopyEachFile(HashMap::new())
                    }
                },
            )]
            .into()
        };

        let LazyType::Opt(events) = lazy_type else {
            return Self {
                pkgid2scripts: vec![PkgId2ScriptsItem {
                    pkgid: id.as_str(),
                    script,
                    start: true,
                }],
                overwrite_files: overwrite_files(id),
                ..Default::default()
            };
        };
        let mut event2pkgid: BTreeMap<Autocmd, Vec<_>> = BTreeMap::new();
        let mut cmd2pkgid: BTreeMap<UserCmd, Vec<_>> = BTreeMap::new();
        let mut ft2pkgid: BTreeMap<FileType, Vec<_>> = BTreeMap::new();
        let mut luam2pkgid: BTreeMap<LuaModule, Vec<_>> = BTreeMap::new();
        let mut keypattern2pkgid: BTreeMap<ModeChar, BTreeMap<Arc<String>, Vec<_>>> =
            BTreeMap::new();

        let pkgid2scripts = vec![PkgId2ScriptsItem {
            pkgid: id.as_str(),
            script,
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
                LuaModule(luam) => {
                    luam2pkgid.entry(luam).or_default().push(id.as_str());
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
            luam2pkgid,
            keypattern2pkgid,
            overwrite_files: overwrite_files(id),
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

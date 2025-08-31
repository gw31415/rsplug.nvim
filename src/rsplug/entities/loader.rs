use std::{
    borrow::Cow,
    collections::{BTreeMap, btree_map::Keys},
    iter::Sum,
    ops::AddAssign,
    path::PathBuf,
    sync::Arc,
};

use hashbrown::HashMap;
use sailfish::TemplateSimple;

use super::*;

/// プラグインの読み込み制御や、ロード後の設定 (lua_source等) にまつわる情報を保持し、Package に変換するための構造体。
#[derive(Default)]
pub struct Loader {
    pkgid2scripts: Vec<(PackageIDStr, SetupScript)>,
    event2pkgid: BTreeMap<Autocmd, Vec<PackageIDStr>>,
    cmd2pkgid: BTreeMap<UserCmd, Vec<PackageIDStr>>,
    ft2pkgid: BTreeMap<FileType, Vec<PackageIDStr>>,
    luam2pkgid: BTreeMap<LuaModule, Vec<PackageIDStr>>,
    keypattern2pkgid: BTreeMap<ModeChar, BTreeMap<Arc<String>, Vec<PackageIDStr>>>,
}

/// 単スクリプトをランタイムパスに配置するためのパッケージを作成する。
fn instant_startup_pkg(path: &str, data: impl Into<Cow<'static, [u8]>>) -> Package {
    let data = data.into();
    let id = PackageID::new(&data) + PackageID::new(path);
    let files = HashMap::from([(
        PathBuf::from(path),
        FileItem {
            source: Arc::new(FileSource::File { data }),
            merge_type: MergeType::Overwrite,
        },
    )]);
    Package {
        id,
        lazy_type: LazyType::Start,
        files,
        script: Default::default(),
    }
}

impl From<Loader> for Vec<Package> {
    fn from(value: Loader) -> Vec<Package> {
        if value.is_empty() {
            return Vec::with_capacity(0);
        }
        let Loader {
            pkgid2scripts,
            event2pkgid,
            cmd2pkgid,
            ft2pkgid,
            luam2pkgid,
            keypattern2pkgid,
        } = value;

        let mut pkgs = Vec::new();

        {
            // Add packages to place scripts that does the initial setup of the plugin
            let pkgid2scripts = pkgid2scripts
                .into_iter()
                .filter_map(|(pkgid, script)| {
                    let mut script_set = BTreeMap::new();
                    let mut add_script = |script_type: &'static str, content: Option<String>| {
                        if let Some(content) = content {
                            let module_id = format!("{script_type}_{pkgid}");
                            pkgs.push(instant_startup_pkg(
                                &format!("lua/{module_id}.lua"),
                                content.into_bytes(),
                            ));
                            script_set.insert(script_type, module_id);
                        }
                    };

                    let SetupScript { lua_source } = script;
                    add_script("lua_source", lua_source);
                    if script_set.is_empty() {
                        None
                    } else {
                        Some((pkgid, script_set))
                    }
                })
                .collect();
            pkgs.push(instant_startup_pkg(
                "lua/_rsplug/init.lua",
                CustomPackaddTemplate { pkgid2scripts }
                    .render_once()
                    .unwrap()
                    .into_bytes(),
            ));
        }

        if !ft2pkgid.is_empty() {
            // on_ft setup
            pkgs.push(instant_startup_pkg(
                "lua/_rsplug/on_ft.lua",
                include_bytes!("../../../templates/lua/_rsplug/on_ft.lua"),
            ));
            for (ft, pkgids) in ft2pkgid {
                let mut path = format!("ftplugin/{ft}/");
                let data = FtpluginTemplate { pkgids, ft }
                    .render_once()
                    .unwrap()
                    .into_bytes();
                path.push_str(&PackageID::new(&data).as_str());
                path.push_str(".lua");

                pkgs.push(instant_startup_pkg(&path, data));
            }
        }

        if !event2pkgid.is_empty() {
            // on_event setup
            pkgs.push({
                let events = event2pkgid.keys();
                let on_event_setup = OnEventSetupTemplate { events }
                    .render_once()
                    .unwrap()
                    .into_bytes()
                    .into();
                let on_event_setup_id = PackageID::new(&on_event_setup);
                let on_event = OnEventTemplate {
                    event2pkgid: &event2pkgid,
                }
                .render_once()
                .unwrap()
                .into_bytes()
                .into();
                let on_event_id = PackageID::new(&on_event);
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
                Package {
                    id: on_event_setup_id + on_event_id,
                    lazy_type: LazyType::Start,
                    files,
                    script: Default::default(),
                }
            });
        }

        if !cmd2pkgid.is_empty() {
            // on_cmd setup
            pkgs.push({
                let cmds = cmd2pkgid.keys();
                let on_cmd_setup = OnCmdSetupTemplate { cmds }
                    .render_once()
                    .unwrap()
                    .into_bytes()
                    .into();
                let on_cmd_setup_id = PackageID::new(&on_cmd_setup);
                let on_cmd = OnCmdTemplate {
                    cmd2pkgid: &cmd2pkgid,
                }
                .render_once()
                .unwrap()
                .into_bytes()
                .into();
                let on_cmd_id = PackageID::new(&on_cmd);
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
                Package {
                    id: on_cmd_id + on_cmd_setup_id,
                    lazy_type: LazyType::Start,
                    files,
                    script: Default::default(),
                }
            });
        }
        if !luam2pkgid.is_empty() {
            let plugin_on_lua = include_bytes!("../../../templates/plugin/on_lua.lua");
            let plugin_on_lua_id = PackageID::new(plugin_on_lua);
            let on_lua = OnLuaTemplate {
                luam2pkgid: &luam2pkgid,
            }
            .render_once()
            .unwrap()
            .into_bytes()
            .into();
            let on_lua_id = PackageID::new(&on_lua);
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
            pkgs.push(Package {
                id: plugin_on_lua_id + on_lua_id,
                lazy_type: LazyType::Start,
                files,
                script: Default::default(),
            });
        }
        if !keypattern2pkgid.is_empty() {
            let data = include_bytes!("../../../templates/plugin/on_map.lua");
            pkgs.push(instant_startup_pkg(
                &format!("plugin/{}.lua", PackageID::new(data).as_str()),
                data,
            ));
            pkgs.push(instant_startup_pkg(
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
                pkgs.push(instant_startup_pkg(
                    &format!("lua/_rsplug/on_map/mode_{mode}.lua"),
                    data,
                ));
            }
        }

        pkgs
    }
}

impl AddAssign for Loader {
    fn add_assign(&mut self, other: Self) {
        let Self {
            pkgid2scripts: scripts,
            event2pkgid,
            cmd2pkgid,
            ft2pkgid,
            luam2pkgid,
            keypattern2pkgid,
        } = other;
        for (event, ids) in event2pkgid {
            self.event2pkgid
                .entry(event)
                .or_default()
                .extend(ids.into_iter());
        }
        self.pkgid2scripts.extend(scripts);
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
    }
}

impl Sum for Loader {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        let mut res = Loader::new();
        for l in iter {
            res += l
        }
        res
    }
}

impl Loader {
    /// Create empty loader
    pub fn new() -> Self {
        Default::default()
    }
    /// Loaderが空かどうか
    pub fn is_empty(&self) -> bool {
        let Self {
            pkgid2scripts: scripts,
            event2pkgid,
            cmd2pkgid,
            ft2pkgid,
            luam2pkgid,
            keypattern2pkgid,
        } = self;
        event2pkgid.is_empty()
            && scripts.is_empty()
            && cmd2pkgid.is_empty()
            && ft2pkgid.is_empty()
            && luam2pkgid.is_empty()
            && keypattern2pkgid.values().all(|v| v.is_empty())
    }
    /// Loaderを Package のベクタに変換する。
    pub fn into_pkgs(self) -> Vec<Package> {
        self.into()
    }
    /// パッケージ情報を読み込み、 Loader を作成する。
    /// 読み込む情報が要らない場合は `None` を返す。
    /// NOTE: Package はインストールされる必要があるため、変更を抑制する意図で PackageID の所有権を奪う。
    /// その他必要な情報のみ引数に取る。
    pub(super) fn create(id: PackageID, lazy_type: LazyType, script: SetupScript) -> Self {
        let LazyType::Opt(events) = lazy_type else {
            return Default::default();
        };
        let mut event2pkgid: BTreeMap<Autocmd, Vec<_>> = BTreeMap::new();
        let mut cmd2pkgid: BTreeMap<UserCmd, Vec<_>> = BTreeMap::new();
        let mut ft2pkgid: BTreeMap<FileType, Vec<_>> = BTreeMap::new();
        let mut luam2pkgid: BTreeMap<LuaModule, Vec<_>> = BTreeMap::new();
        let mut keypattern2pkgid: BTreeMap<ModeChar, BTreeMap<Arc<String>, Vec<_>>> =
            BTreeMap::new();

        let id = Arc::new(id);
        let pkgid2scripts = Vec::from([(id.as_str(), script)]);
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
        }
    }
}

#[derive(TemplateSimple)]
#[template(path = "ftplugin/on_ft.stpl")]
#[template(escape = false)]
struct FtpluginTemplate {
    pkgids: Vec<PackageIDStr>,
    ft: FileType,
}

#[derive(TemplateSimple)]
#[template(path = "lua/_rsplug/init.stpl")]
#[template(escape = false)]
struct CustomPackaddTemplate {
    pkgid2scripts: Vec<(PackageIDStr, BTreeMap<&'static str, String>)>,
}

#[derive(TemplateSimple)]
#[template(path = "plugin/on_event.stpl")]
#[template(escape = false)]
struct OnEventSetupTemplate<'a> {
    events: Keys<'a, Autocmd, Vec<PackageIDStr>>,
}

#[derive(TemplateSimple)]
#[template(path = "lua/_rsplug/on_event.stpl")]
#[template(escape = false)]
struct OnEventTemplate<'a> {
    event2pkgid: &'a BTreeMap<Autocmd, Vec<PackageIDStr>>,
}

#[derive(TemplateSimple)]
#[template(path = "plugin/on_cmd.stpl")]
#[template(escape = false)]
struct OnCmdSetupTemplate<'a> {
    cmds: Keys<'a, UserCmd, Vec<PackageIDStr>>,
}

#[derive(TemplateSimple)]
#[template(path = "lua/_rsplug/on_cmd.stpl")]
#[template(escape = false)]
struct OnCmdTemplate<'a> {
    cmd2pkgid: &'a BTreeMap<UserCmd, Vec<PackageIDStr>>,
}

#[derive(TemplateSimple)]
#[template(path = "lua/_rsplug/on_lua.stpl")]
#[template(escape = false)]
struct OnLuaTemplate<'a> {
    luam2pkgid: &'a BTreeMap<LuaModule, Vec<PackageIDStr>>,
}

#[derive(TemplateSimple)]
#[template(path = "lua/_rsplug/on_map/mode__.stpl")]
#[template(escape = false)]
struct OnMapTemplate<'a> {
    mode: &'a ModeChar,
    keypattern2pkgid: &'a BTreeMap<ModeChar, BTreeMap<Arc<String>, Vec<PackageIDStr>>>,
}

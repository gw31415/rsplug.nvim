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
    autocmds: BTreeMap<String, Vec<PackageIDStr>>,
    scripts: Vec<(PackageIDStr, SetupScript)>,
    cmd_id_map: BTreeMap<String, PackageIDStr>,
}

/// 単スクリプトをランタイムパスに配置するためのパッケージを作成する。
fn instant_startup_pkg(path: &str, data: impl Into<Cow<'static, [u8]>>) -> Package {
    let data = data.into();
    let id = PackageID::new(&data) + PackageID::new(path);
    let files = HashMap::from([(PathBuf::from(path), Arc::new(FileSource::File { data }))]);
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
            autocmds,
            scripts,
            cmd_id_map,
        } = value;

        let mut pkgs = vec![
            // Add the basic lazy loading modules
            instant_startup_pkg(
                "lua/_rsplug/init.lua",
                include_bytes!("../../../lua/_rsplug/init.lua"),
            ),
        ];

        if !scripts.is_empty() {
            // Add packages to place scripts that does the initial setup of the plugin
            let scripts = scripts
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
                "plugin/_rsplug_setup_scripts.lua",
                SetupScriptsTemplate { scripts }
                    .render_once()
                    .unwrap()
                    .into_bytes(),
            ));
        }

        if !autocmds.is_empty() {
            // Add autocmd setup
            pkgs.push({
                let data = AutocmdTemplate {
                    autocmds: &autocmds,
                }
                .render_once()
                .unwrap()
                .into_bytes()
                .into();
                let id = PackageID::new(&data);
                let files = HashMap::from([(
                    PathBuf::from(format!("plugin/{}.lua", id.as_str())),
                    Arc::new(FileSource::File { data }),
                )]);
                Package {
                    id,
                    lazy_type: LazyType::Start,
                    files,
                    script: Default::default(),
                }
            });
        }

        if !cmd_id_map.is_empty() {
            // Add autocmd setup
            pkgs.push({
                let cmds = cmd_id_map.keys();
                let command = CommandTemplate { cmds }
                    .render_once()
                    .unwrap()
                    .into_bytes()
                    .into();
                let command_id = PackageID::new(&command);
                let lazycmd = LazycmdTemplate {
                    cmd_id_map: &cmd_id_map,
                }
                .render_once()
                .unwrap()
                .into_bytes()
                .into();
                let lazycmd_id = PackageID::new(&lazycmd);
                let files = HashMap::from([
                    (
                        PathBuf::from("lua/_rsplug/lazycmd.lua"),
                        Arc::new(FileSource::File { data: lazycmd }),
                    ),
                    (
                        PathBuf::from(format!("plugin/{}.lua", command_id.as_str())),
                        Arc::new(FileSource::File { data: command }),
                    ),
                ]);
                Package {
                    id: lazycmd_id + command_id,
                    lazy_type: LazyType::Start,
                    files,
                    script: Default::default(),
                }
            });
        }

        pkgs
    }
}

impl AddAssign for Loader {
    fn add_assign(&mut self, other: Self) {
        let Self {
            autocmds,
            scripts,
            cmd_id_map,
        } = other;
        for (event, ids) in autocmds {
            self.autocmds
                .entry(event)
                .or_default()
                .extend(ids.into_iter());
        }
        self.scripts.extend(scripts);
        self.cmd_id_map.extend(cmd_id_map);
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
            autocmds,
            cmd_id_map,
            scripts,
        } = self;
        autocmds.is_empty() && scripts.is_empty() && cmd_id_map.is_empty()
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
        let mut autocmds: BTreeMap<String, Vec<_>> = BTreeMap::new();
        let mut cmd_id_map: BTreeMap<String, PackageIDStr> = BTreeMap::new();

        let id = Arc::new(id);
        let scripts = Vec::from([(id.as_str(), script)]);
        for ev in events {
            use LoadEvent::*;
            match ev {
                Autocmd(autocmd) => {
                    autocmds.entry(autocmd).or_default().push(id.as_str());
                }
                Cmd(cmd) => {
                    cmd_id_map.insert(cmd, id.as_str());
                }
            }
        }
        Self {
            autocmds,
            scripts,
            cmd_id_map,
        }
    }
}

#[derive(TemplateSimple)]
#[template(path = "autocmd.stpl")]
#[template(escape = false)]
struct AutocmdTemplate<'a> {
    autocmds: &'a BTreeMap<String, Vec<PackageIDStr>>,
}

#[derive(TemplateSimple)]
#[template(path = "setup_scripts.stpl")]
#[template(escape = false)]
struct SetupScriptsTemplate {
    scripts: Vec<(PackageIDStr, BTreeMap<&'static str, String>)>,
}

#[derive(TemplateSimple)]
#[template(path = "command.stpl")]
#[template(escape = false)]
struct CommandTemplate<'a> {
    cmds: Keys<'a, String, PackageIDStr>,
}

#[derive(TemplateSimple)]
#[template(path = "lazycmd.stpl")]
#[template(escape = false)]
struct LazycmdTemplate<'a> {
    cmd_id_map: &'a BTreeMap<String, PackageIDStr>,
}

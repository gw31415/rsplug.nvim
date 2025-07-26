use std::{ops::AddAssign, path::PathBuf, sync::Arc};

use hashbrown::HashMap;
use sailfish::TemplateSimple;

use super::*;

/// プラグインの読み込み制御や、ロード後の設定 (lua_source等) にまつわる情報を保持し、Package に変換するための構造体。
pub struct Loader {
    autocmds: HashMap<String, Vec<PackageIDStr>>,
}

impl From<Loader> for Vec<Package> {
    fn from(value: Loader) -> Vec<Package> {
        let mut pkgs = Vec::new();
        if !value.autocmds.is_empty() {
            pkgs.push({
                let data = include_bytes!("../../lua/_rsplug/autocmd.lua").into();

                let id = PackageID::new(&data);
                let files = HashMap::from([(
                    PathBuf::from("lua/_rsplug/autocmd.lua"),
                    Arc::new(FileSource::File { data }),
                )]);
                Package {
                    id,
                    lazy_type: LazyType::Start,
                    files,
                }
            });

            pkgs.push({
                let data = value.lua_code().into_bytes().into();
                let id = PackageID::new(&data);
                let files = HashMap::from([(
                    PathBuf::from(format!("plugin/{}.lua", id.as_str())),
                    Arc::new(FileSource::File { data }),
                )]);
                Package {
                    id,
                    lazy_type: LazyType::Start,
                    files,
                }
            });
        }
        pkgs
    }
}

impl AddAssign for Loader {
    fn add_assign(&mut self, other: Self) {
        for (event, ids) in other.autocmds {
            self.autocmds
                .entry(event)
                .or_default()
                .extend(ids.into_iter());
        }
    }
}

impl Loader {
    /// パッケージ情報を読み込み、 Loader を作成する。
    /// 読み込む情報が要らない場合は `None` を返す。
    /// NOTE: Package はインストールされる必要があるため、変更を抑制する意図で PackageID の所有権を奪う。
    /// その他必要な情報のみ引数に取る。
    pub(super) fn create(id: PackageID, lazy_type: LazyType) -> Option<Self> {
        let LazyType::Opt(events) = lazy_type else {
            return None;
        };
        let mut autocmds: HashMap<String, Vec<_>> = HashMap::new();

        let id = Arc::new(id);
        for ev in events {
            use LoadEvent::*;
            match ev {
                Autocmd(autocmd) => {
                    autocmds.entry(autocmd).or_default().push(id.as_str());
                }
            }
        }
        Some(Self { autocmds })
    }

    fn lua_code(&self) -> String {
        let Self { autocmds } = self;
        LoaderLuaTemplate { autocmds }.render_once().unwrap()
    }
}

#[derive(TemplateSimple)]
#[template(path = "loader_lua.stpl")]
#[template(escape = false)]
struct LoaderLuaTemplate<'a> {
    autocmds: &'a HashMap<String, Vec<PackageIDStr>>,
}

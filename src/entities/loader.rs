use std::{ops::AddAssign, path::PathBuf, sync::Arc};

use hashbrown::HashMap;
use itertools::Itertools;
use sailfish::TemplateSimple;

use super::{FileSource, LoadEvent, Package, PackageID, PackageIDStr, PackageType};

pub struct Loader {
    autocmds: HashMap<String, Vec<Arc<PackageID>>>,
}

impl From<Loader> for Vec<Package> {
    fn from(value: Loader) -> Self {
        let mut pkgs = Vec::new();
        if !value.autocmds.is_empty() {
            pkgs.push({
                let data = include_bytes!("../../lua/autocmd.lua").into();

                let id = PackageID::new(&data);
                let files = HashMap::from([(
                    PathBuf::from("lua/_rsplug/autocmd.lua"),
                    Arc::new(FileSource::File { data }),
                )]);
                Package {
                    id,
                    package_type: PackageType::Start,
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
                    package_type: PackageType::Start,
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
    pub(super) fn create(id: PackageID, package_type: PackageType) -> Option<Self> {
        let PackageType::Opt(events) = package_type else {
            return None;
        };
        let mut autocmds: HashMap<String, Vec<_>> = HashMap::new();

        let id = Arc::new(id);
        for ev in events {
            use LoadEvent::*;
            match ev {
                Autocmd(autocmd) => {
                    autocmds.entry(autocmd).or_default().push(id.clone());
                }
            }
        }
        Some(Self { autocmds })
    }

    fn lua_code(&self) -> String {
        self.autocmds
            .iter()
            .map(|(event, ids)| {
                let ids = ids
                    .iter()
                    .map(|id| id.as_str())
                    .collect::<Vec<PackageIDStr>>();
                Autocmd { event, ids }.render_once().unwrap()
            })
            .join("\n")
    }
}

#[derive(sailfish::TemplateSimple)]
#[template(path = "loader_lua.stpl")]
struct Autocmd<'a> {
    event: &'a String,
    ids: Vec<PackageIDStr>,
}

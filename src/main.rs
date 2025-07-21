use std::{io::Write, sync::Arc};

use rsplug::{GlobalConfig, Package, PackageType, Unit, UnitSource};

#[tokio::main]
async fn main() {
    let config = Arc::new(GlobalConfig::default());
    let pkgs = Unit::unpack(
        [
            "vim-denops/denops.vim",
            "lambdalisue/fern-hijack.vim",
            "gw31415/mstdn-editor.vim",
            "gw31415/edisch.vim",
        ]
        .into_iter()
        .map(|repo| {
            let (owner, repo) = repo.split_once('/').unwrap();
            Unit {
                source: UnitSource::GitHub {
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                    rev: None,
                },
                package_type: PackageType::Start,
                depends: vec![],
            }
        }),
        false, // INSTALL or not
        false, // UPDATE or not
        config.clone(),
    )
    .await
    .unwrap();
    println!("Packages: {}", pkgs.len());
    let pkgs = Package::merge(pkgs, config.clone());
    println!("Packages: {}", pkgs.len());
    Package::install(pkgs, config).await.unwrap();
    std::io::stdout().flush().unwrap();
}

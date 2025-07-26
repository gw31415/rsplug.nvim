use std::{
    collections::{BTreeSet, BinaryHeap},
    io::Write,
    sync::Arc,
};

use clap::Parser;
use rsplug::*;

#[derive(clap::Parser, Debug)]
#[command(about)]
struct Args {
    /// Install plugins
    #[arg(short, long)]
    install: bool,
    /// Update plugins
    #[arg(short, long)]
    update: bool,
}

fn github(repo: &str) -> UnitSource {
    let (owner, repo) = repo.split_once('/').unwrap();
    UnitSource::GitHub {
        owner: owner.to_string(),
        repo: repo.to_string(),
        rev: None,
    }
}

#[tokio::main]
async fn main() {
    let Args { install, update } = Args::parse();
    let config = Arc::new(Config::default());
    let mut pkgs: BinaryHeap<_> = Unit::unpack(
        [
            Unit {
                source: github("vim-denops/denops.vim"),
                package_type: PackageType::Start,
                depends: vec![],
            },
            Unit {
                source: github("lambdalisue/fern-hijack.vim"),
                package_type: PackageType::Start,
                depends: vec![],
            },
            Unit {
                source: github("gw31415/mstdn-editor.vim"),
                package_type: PackageType::Start,
                depends: vec![],
            },
            Unit {
                source: github("gw31415/edisch.vim"),
                package_type: PackageType::Start,
                depends: vec![],
            },
            Unit {
                source: github("gw31415/mkdir.vim"),
                package_type: PackageType::Opt(BTreeSet::from([LoadEvent::Autocmd(
                    "BufWritePre".to_string(),
                )])),
                depends: vec![],
            },
        ],
        install, // INSTALL or not
        update,  // UPDATE or not
        config.clone(),
    )
    .await
    .expect("Failed to parse units");
    println!("Total Packages: {}", pkgs.len());

    let mut state = PackPathState::new();
    Package::merge(&mut pkgs);
    while let Some(pkg) = pkgs.pop() {
        for pkg in state.insert(pkg) {
            pkgs.push(pkg);
        }
        Package::merge(&mut pkgs);
    }
    state.install(&config.packpath).await.unwrap();
    std::io::stdout().flush().unwrap();
}

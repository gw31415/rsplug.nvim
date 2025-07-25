use std::{collections::BinaryHeap, io::Write, sync::Arc};

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

#[tokio::main]
async fn main() {
    let Args { install, update } = Args::parse();
    let config = Arc::new(Config::default());
    let mut pkgs: BinaryHeap<_> = Unit::unpack(
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
        install, // INSTALL or not
        update,  // UPDATE or not
        config.clone(),
    )
    .await
    .expect("Failed to parse units");
    println!("Total Packages: {}", pkgs.len());
    Package::merge(&mut pkgs);
    println!("Merge Packages: {}", pkgs.len());

    let mut state = PackPathState::new();
    while let Some(pkg) = pkgs.pop() {
        let Some(pkg) = state.insert(pkg) else {
            continue;
        };
        pkgs.push(pkg);
        Package::merge(&mut pkgs);
    }
    state.install(&config.packpath).await.unwrap();
    std::io::stdout().flush().unwrap();
}

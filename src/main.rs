use std::{
    collections::{BTreeSet, BinaryHeap},
    io::Write,
    path::PathBuf,
};

use clap::Parser;
use once_cell::sync::Lazy;
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

static DEFAULT_APP_DIR: Lazy<PathBuf> = Lazy::new(|| {
    let homedir = std::env::home_dir().unwrap();
    let cachedir = homedir.join(".cache");
    cachedir.join("rsplug")
});

#[tokio::main]
async fn main() {
    let Args { install, update } = Args::parse();

    let units = [
        Unit {
            source: github("vim-denops/denops.vim"),
            lazy_type: LazyType::Start,
            depends: vec![],
        },
        Unit {
            source: github("lambdalisue/fern-hijack.vim"),
            lazy_type: LazyType::Start,
            depends: vec![],
        },
        Unit {
            source: github("gw31415/mstdn-editor.vim"),
            lazy_type: LazyType::Start,
            depends: vec![],
        },
        Unit {
            source: github("gw31415/edisch.vim"),
            lazy_type: LazyType::Start,
            depends: vec![],
        },
        Unit {
            source: github("gw31415/mkdir.vim"),
            lazy_type: LazyType::Opt(BTreeSet::from([LoadEvent::Autocmd(
                "BufWritePre".to_string(),
            )])),
            depends: vec![],
        },
    ];

    let mut pkgs = Cache::new(DEFAULT_APP_DIR.as_path())
        .install::<BinaryHeap<_>>(units, install, update)
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

    state.install(DEFAULT_APP_DIR.as_path()).await.unwrap();
    std::io::stdout().flush().unwrap();
}

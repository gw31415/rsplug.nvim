use std::{collections::BinaryHeap, io::Write, path::PathBuf};

use clap::Parser;
use once_cell::sync::Lazy;
use rsplug::*;
use tokio::task::JoinSet;

#[derive(clap::Parser, Debug)]
#[command(about)]
struct Args {
    /// Install plugins
    #[arg(short, long)]
    install: bool,
    /// Update plugins
    #[arg(short, long)]
    update: bool,
    /// Config files to process
    #[arg()]
    paths: Vec<PathBuf>,
}

static DEFAULT_APP_DIR: Lazy<PathBuf> = Lazy::new(|| {
    let homedir = std::env::home_dir().unwrap();
    let cachedir = homedir.join(".cache");
    cachedir.join("rsplug")
});

#[tokio::main]
async fn main() {
    let Args {
        install,
        update,
        paths,
    } = Args::parse();

    let units = {
        let config = paths
            .into_iter()
            .map(tokio::fs::read_to_string)
            .collect::<JoinSet<_>>()
            .join_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("Could not load some configuration files")
            .into_iter()
            .map(|content| toml::from_str::<Config>(&content).unwrap())
            .sum();
        Unit::new(config).unwrap()
    };

    let mut pkgs: BinaryHeap<_> = Cache::new(DEFAULT_APP_DIR.as_path())
        .install(units, install, update)
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

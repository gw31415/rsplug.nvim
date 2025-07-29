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
    config_files: Vec<PathBuf>,
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
        config_files,
    } = Args::parse();

    let units = {
        // parse config files into Vec<Arc<Unit>>
        let configs = config_files
            .into_iter()
            .map(|path| async {
                let content = tokio::fs::read_to_string(path).await?;
                let config = toml::from_str::<Config>(&content)?;
                Ok::<_, Error>(config)
            })
            .collect::<JoinSet<_>>()
            .join_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("Some config files failed to parse")
            .into_iter();
        Unit::new(configs.sum())
    };

    // Fetch packages through Cache based on the Units
    let mut pkgs: BinaryHeap<_> = Cache::new(DEFAULT_APP_DIR.as_path())
        .fetch(units, install, update)
        .await
        .expect("Failed to parse units");
    println!("Total Packages: {}", pkgs.len());

    // Create PackPathState and insert packages into it
    let mut state = PackPathState::new();
    let loader = &mut Loader::new();
    Package::merge(&mut pkgs);
    while let Some(pkg) = pkgs.pop() {
        // Merging more by accumulating Loader until all the rest of the pkgs are Start
        if pkg.lazy_type.is_start() && !loader.is_empty() {
            pkgs.push(pkg);
            pkgs.extend(std::mem::take(loader).into_pkgs());
            Package::merge(&mut pkgs);
            continue;
        }

        *loader += state.insert(pkg);
    }

    // Install the packages into the packpath.
    state.install(DEFAULT_APP_DIR.as_path()).await.unwrap();
    std::io::stdout().flush().unwrap();
}

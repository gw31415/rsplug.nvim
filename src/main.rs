mod entities;
mod util;

mod rsplug {
    use super::*;

    pub use entities::error;
    // pub use entities::lazy_type;
    pub use entities::package;
    pub use entities::unit;

    pub use entities::cache::Cache;
    pub use entities::config::Config;
    pub use entities::loader::Loader;
    // pub use lazy_type::{LazyType, LoadEvent};
    pub use package::{PackPathState, Package};
    pub use unit::{Unit /*UnitSource*/};
}

use std::{collections::BinaryHeap, io::Write, path::PathBuf};

use clap::Parser;
use once_cell::sync::Lazy;
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
                let content = tokio::fs::read_to_string(path).await.expect("System Error");
                toml::from_str::<rsplug::Config>(&content)
            })
            .collect::<JoinSet<_>>()
            .join_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("Some config files failed to parse")
            .into_iter();
        rsplug::Unit::new(configs.sum())
    };

    // Fetch packages through Cache based on the Units
    let mut pkgs: BinaryHeap<_> = rsplug::Cache::new(DEFAULT_APP_DIR.as_path())
        .fetch(units, install, update)
        .await
        .expect("System Error");
    println!("Total Packages: {}", pkgs.len());

    // Create PackPathState and insert packages into it
    let mut state = rsplug::PackPathState::new();
    let mut loader = rsplug::Loader::new();
    rsplug::Package::merge(&mut pkgs);
    while let Some(pkg) = pkgs.pop() {
        // Merging more by accumulating Loader until all the rest of the pkgs are Start
        if pkg.lazy_type.is_start() && !loader.is_empty() {
            pkgs.push(pkg);
            pkgs.extend(std::mem::take(&mut loader).into_pkgs());
            rsplug::Package::merge(&mut pkgs);
            continue;
        }

        loader += state.insert(pkg);
    }

    // Install the packages into the packpath.
    state.install(DEFAULT_APP_DIR.as_path()).await.unwrap();
    std::io::stdout().flush().unwrap();
}

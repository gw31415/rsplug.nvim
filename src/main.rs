use std::{collections::BinaryHeap, path::PathBuf};

use clap::Parser;
use log::{Message, close, msg};
use once_cell::sync::Lazy;
use tokio::task::JoinSet;

mod log;
mod rsplug;

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
    #[arg(required = true, env = "RSPLUG_CONFIG_FILES", value_delimiter = ':')]
    config_files: Vec<String>,
}

async fn app() -> Result<(), Error> {
    let Args {
        install,
        update,
        config_files,
    } = Args::parse();

    let units = {
        // parse config files into Iterator<Item = Arc<Unit>>
        let configs = config_files
            .into_iter()
            .map(|path| async {
                let content = tokio::fs::read(&path).await.map_err(Error::Io)?;
                let config: rsplug::Config =
                    toml::from_slice(&content).map_err(|e| Error::Parse(e, path.into()))?;
                Ok::<_, Error>(config)
            })
            .collect::<JoinSet<_>>()
            .join_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?
            .into_iter();
        rsplug::Unit::new(configs.sum())?
    };

    // Fetch packages through Cache based on the Units
    let mut pkgs: BinaryHeap<_> = rsplug::Cache::new(DEFAULT_APP_DIR.as_path())
        .fetch(units, install, update)
        .await?
        .collect();
    msg(Message::TotalPackages(pkgs.len()));

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
    msg(Message::TotalPackagesMerged(state.len()));

    // Install the packages into the packpath.
    state
        .install(DEFAULT_APP_DIR.as_path())
        .await
        .map_err(rsplug::Error::Io)?;
    Ok(())
}

static DEFAULT_APP_DIR: Lazy<PathBuf> = Lazy::new(|| {
    let homedir = std::env::home_dir().unwrap();
    let cachedir = homedir.join(".cache");
    cachedir.join("rsplug")
});

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("failed to parse {1:?}: {0}")]
    Parse(toml::de::Error, PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Rsplug(#[from] rsplug::Error),
    #[error(transparent)]
    Dag(#[from] rsplug::unit::DAGCreationError),
}

#[tokio::main]
async fn main() {
    if let Err(e) = app().await {
        msg(Message::Error(e.into()));
        close(1).await;
    }
    close(0).await;
}

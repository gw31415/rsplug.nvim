mod log;
mod rsplug;

use std::{collections::BinaryHeap, path::PathBuf};

use clap::Parser;
use log::{Message, close, msg};
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
    /// Glob-patterns of the config files. Split by ':' to specify multiple patterns
    #[arg(
        required = true,
        env = "RSPLUG_CONFIG_FILES",
        value_delimiter = ':',
        hide_env_values = true
    )]
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
        let configs = rsplug::util::glob::find(config_files.iter().map(String::as_str))?
            .filter_map(|path| match path {
                Err(e) => Some(Err(e)),
                Ok(path) => {
                    if path.is_file() {
                        Some(Ok(path.to_path_buf()))
                    } else {
                        None
                    }
                }
            })
            .map(|path| async {
                let path = path?;
                let content = tokio::fs::read(&path).await.map_err(Error::Io)?;

                match toml::from_slice::<rsplug::Config>(&content) {
                    Ok(config) => {
                        log::msg(Message::DetectConfigFile(path));
                        Ok(config)
                    }
                    Err(e) => Err(Error::Parse(e, path)),
                }
            })
            .collect::<JoinSet<_>>()
            .join_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, Error>>()?
            .into_iter();
        rsplug::Unit::new(configs.sum())?
    };

    msg(Message::CheckingLocalPlugins { install, update });
    // Fetch packages through Cache based on the Units
    let mut pkgs = {
        let res = units
            .map(|unit| unit.fetch(install, update, DEFAULT_APP_DIR.as_path()))
            .collect::<JoinSet<_>>()
            .join_all()
            .await;
        msg(Message::CacheDone);
        res.into_iter()
            .try_fold(BinaryHeap::new(), |mut acc, res| {
                if let Some(pkg) = res? {
                    acc.push(pkg);
                }
                Ok::<_, Error>(acc)
            })?
    };
    let total_count = pkgs.len();

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
    msg(Message::CheckingLocalPluginsFinished {
        total: total_count,
        merged: state.len(),
    });

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
    Dag(#[from] dag::DagError),
    #[error(transparent)]
    Ignore(#[from] ignore::Error),
}

#[tokio::main]
async fn main() {
    if let Err(e) = app().await {
        msg(Message::Error(e.into()));
        close(1).await;
    }
    close(0).await;
}

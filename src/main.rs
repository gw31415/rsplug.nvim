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

    let plugins = {
        // Parse all of config files
        // NOTE: Wait for all config files to parse.
        let configs = {
            let mut joinset = rsplug::util::glob::find(config_files.iter().map(String::as_str))?
                .filter_map(|path| match path {
                    Err(e) => Some(Err(e)),
                    Ok(path) => {
                        if path.is_dir() {
                            None
                        } else {
                            Some(Ok(path.to_path_buf()))
                        }
                    }
                })
                .map(|path| async {
                    let path = path?;
                    let content = tokio::fs::read(&path).await?;

                    match toml::from_slice::<rsplug::Config>(&content) {
                        Ok(config) => {
                            log::msg(Message::DetectConfigFile(path.to_path_buf()));
                            Ok(config)
                        }
                        Err(e) => Err(Error::Parse(e, path.to_path_buf())),
                    }
                })
                .collect::<JoinSet<_>>();
            let mut confs = Vec::new();
            while let Some(config) = joinset.join_next().await {
                confs
                    .push(config.expect(
                        "Some tasks reading config files may be unintentionally aborted",
                    )?);
            }
            confs
        };
        rsplug::Plugin::new(configs.into_iter().sum())?
    };

    msg(Message::Loading { install, update });
    // Load plugins through Cache based on the Units
    let mut plugins = {
        let res = plugins
            .map(|plugin| plugin.load(install, update, DEFAULT_REPOCACHE_DIR.as_path()))
            .collect::<JoinSet<_>>()
            .join_all()
            .await;
        // Wait until all loading is complete.
        // NOTE: It does not abort if an error occurs (because of the build process).
        msg(Message::LoadDone);
        res.into_iter()
            .try_fold(BinaryHeap::new(), |mut acc, res| {
                if let Some(pkg) = res? {
                    acc.push(pkg);
                }
                Ok::<_, Error>(acc)
            })?
    };
    let total_count = plugins.len();

    // Create PackPathState and insert packages into it
    let mut state = rsplug::PackPathState::new();
    let mut loader = rsplug::Loader::new();
    rsplug::PluginLoaded::merge(&mut plugins);
    while let Some(pkg) = plugins.pop() {
        // Merging more by accumulating Loader until all the rest of the pkgs are Start
        if pkg.lazy_type.is_start() && !loader.is_empty() {
            plugins.push(pkg);
            plugins.extend(std::mem::take(&mut loader).into_pkgs());
            rsplug::PluginLoaded::merge(&mut plugins);
            continue;
        }

        loader += state.insert(pkg);
    }
    msg(Message::MergeFinished {
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
    let homedir = std::env::home_dir().expect("Failed to get home directory");
    let cachedir = homedir.join(".cache");
    cachedir.join("rsplug")
});

static DEFAULT_REPOCACHE_DIR: Lazy<PathBuf> = Lazy::new(|| DEFAULT_APP_DIR.join("repos"));

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

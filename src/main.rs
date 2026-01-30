mod log;
mod rsplug;

use std::{
    collections::{BTreeMap, BinaryHeap},
    path::PathBuf,
    sync::Arc,
};

use clap::Parser;
use log::{Message, close, msg};
use once_cell::sync::Lazy;
use tokio::task::JoinSet;

use crate::rsplug::{LockFile, plugin::PluginLockInfo};

#[derive(clap::Parser, Debug)]
#[command(about)]
struct Args {
    /// Install plugins
    #[arg(short, long)]
    install: bool,
    /// Update plugins
    #[arg(short, long)]
    update: bool,
    /// Frozen lockfile mode: only use the lockfile and do not update it
    #[arg(long)]
    frozen_lockfile: bool,
    /// Specify the lockfile path
    #[arg(short, long)]
    lockfile: Option<PathBuf>,
    /// Glob-patterns of the config files. Split by ':' to specify multiple patterns
    #[arg(
        required_unless_present = "frozen_lockfile",
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
        lockfile,
        frozen_lockfile,
        config_files,
    } = Args::parse();
    let lockfile = Arc::new(lockfile.unwrap_or(DEFAULT_APP_DIR.join("rsplug.lock.json")));

    let (plugins, locked, config) = if frozen_lockfile {
        // Build from lock file
        let rsplug::LockFile {
            plugins, locked, ..
        } = rsplug::LockFile::read(lockfile.as_path()).await?;
        msg(Message::DetectLockFile(lockfile.clone()));

        let config = rsplug::Config { plugins };
        (rsplug::Plugin::new(config.clone())?, locked, config)
    } else {
        // Build from TOML files (existing behavior)
        // Parse all of config files
        let config = {
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
            while let Some(result) = joinset.join_next().await {
                let config = result
                    .expect("Some tasks reading config files may be unintentionally aborted")?;
                confs.push(config);
            }
            // Aggregate all configs and extract plugin configs
            confs.into_iter().sum::<rsplug::Config>()
        };

        (
            rsplug::Plugin::new(config.clone())?,
            BTreeMap::new(),
            config,
        )
    };

    msg(Message::Loading { install, update });
    // Load plugins through Cache based on the Units
    let (mut plugins, lock_infos) = {
        let locked = Arc::new(locked);
        let res = plugins
            .map(|plugin| {
                let locked = Arc::clone(&locked);
                async move {
                    let url = plugin.cache.repo.url();
                    let locked_rev = if let Some(entry) = locked.get(&url) {
                        if entry.kind != rsplug::LockedResourceType::Git {
                            return Err(Error::Io(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!("Unsupported lock type for {}: {:?}", url, entry.kind),
                            )));
                        }
                        Some(Arc::<str>::from(entry.rev.as_str()))
                    } else if install || update {
                        if frozen_lockfile {
                            return Err(Error::Io(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!("Missing locked revision for {}", url),
                            )));
                        }
                        None
                    } else {
                        None
                    };
                    let loaded = plugin
                        .load(install, update, DEFAULT_REPOCACHE_DIR.as_path(), locked_rev)
                        .await?;
                    Ok(loaded)
                }
            })
            .collect::<JoinSet<_>>()
            .join_all()
            .await;
        // Wait until all loading is complete.
        // NOTE: It does not abort if an error occurs (because of the build process).
        msg(Message::LoadDone);
        res.into_iter().try_fold(
            (BinaryHeap::new(), Vec::new()),
            |(mut plugins, mut locks), res| {
                let result = res?;
                if let Some(loaded_plugin) = result.loaded {
                    plugins.push(loaded_plugin);
                }
                if let Some(lock_info) = result.lock_info {
                    locks.push(lock_info);
                }
                Ok::<_, Error>((plugins, locks))
            },
        )?
    };
    let total_count = plugins.len();

    if install || update {
        LockFile {
            version: "1".into(),
            locked: lock_infos
                .into_iter()
                .map(|PluginLockInfo { url, resolved_rev }| {
                    (
                        url,
                        rsplug::LockedResource {
                            kind: rsplug::LockedResourceType::Git,
                            rev: resolved_rev,
                        },
                    )
                })
                .collect(),
            plugins: config.plugins,
        }
        .write(lockfile.as_path())
        .await?;
    }

    // Create PackPathState and insert packages into it
    let mut state = rsplug::PackPathState::new();
    rsplug::LoadedPlugin::merge(&mut plugins);
    for plugin in plugins {
        state.insert(plugin);
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

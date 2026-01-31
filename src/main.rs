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

use crate::rsplug::LockFile;

#[derive(clap::Parser, Debug)]
#[command(about)]
struct Args {
    /// Install plugins
    #[arg(short, long)]
    install: bool,
    /// Update plugins
    #[arg(short, long)]
    update: bool,
    /// Fix the repo version with rev in the lockfile
    #[arg(long)]
    locked: bool,
    /// Do not access remote
    #[arg(long)]
    offline: bool,
    /// Specify the lockfile path
    #[arg(short, long)]
    lockfile: Option<PathBuf>,
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
        lockfile,
        locked,
        offline,
        config_files,
    } = Args::parse();
    let lockfile = lockfile.unwrap_or(DEFAULT_APP_DIR.join("rsplug.lock.json"));

    // Parse all of config files
    let config = rsplug::util::glob::find(config_files.iter().map(String::as_str))?
        .filter_map(|path| match path {
            Err(e) => Some(Err(e)),
            Ok(path) => (!path.is_dir()).then_some(Ok(path.to_path_buf())),
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
        .collect::<JoinSet<_>>()
        .join_all()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("Some tasks reading config files may be unintentionally aborted")
        .into_iter()
        .sum::<rsplug::Config>();

    let locked_map = if locked || !update {
        match rsplug::LockFile::read(lockfile.as_path()).await {
            Ok(rsplug::LockFile { locked, .. }) => {
                msg(Message::DetectLockFile(lockfile.clone()));
                locked
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !locked => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        }
    } else {
        BTreeMap::new()
    };

    let plugins = rsplug::Plugin::new(config)?;

    msg(Message::Loading { install, update });
    // Load plugins through Cache based on the Units
    let locked_map = Arc::new(locked_map);
    let (mut plugins, lock_infos) = {
        let res = plugins
            .map(|plugin| {
                let locked_map = Arc::clone(&locked_map);
                async move {
                    let url = plugin.cache.repo.url();
                    let locked_rev = if locked {
                        if let Some(entry) = locked_map.get(&url) {
                            if entry.kind != rsplug::LockedResourceType::Git {
                                return Err(Error::Io(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    format!("Unsupported lock type for {}: {:?}", url, entry.kind),
                                )));
                            }
                            Some(Arc::<str>::from(entry.rev.as_str()))
                        } else {
                            return Err(Error::Io(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!("Missing locked revision for {}", url),
                            )));
                        }
                    } else {
                        None
                    };
                    let loaded = plugin
                        .load(
                            install,
                            update,
                            offline,
                            DEFAULT_REPOCACHE_DIR.as_path(),
                            locked_rev,
                        )
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
        let (plugins, locks) = res.into_iter().try_fold(
            (BinaryHeap::new(), Vec::new()),
            |(mut plugins, mut locks), res| {
                if let Some((loaded, lock_info)) = res? {
                    plugins.push(loaded);
                    locks.push(lock_info);
                }
                Ok::<_, Error>((plugins, locks))
            },
        )?;
        (plugins, locks)
    };
    let total_count = plugins.len();

    if !locked {
        let mut merged_locked =
            Arc::try_unwrap(locked_map).expect("No other references to locked_map");
        for (url, resolved_rev) in lock_infos {
            merged_locked.insert(
                url,
                rsplug::LockedResource {
                    kind: rsplug::LockedResourceType::Git,
                    rev: resolved_rev,
                },
            );
        }
        LockFile {
            version: "1".into(),
            locked: merged_locked,
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

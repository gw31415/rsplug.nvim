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
    /// Path to lock file. When provided, build from lock file instead of TOML configs
    #[arg(short, long)]
    lock_file: Option<PathBuf>,
    /// Glob-patterns of the config files. Split by ':' to specify multiple patterns
    #[arg(
        required_unless_present = "lock_file",
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
        lock_file,
        config_files,
    } = Args::parse();

    let (plugins, toml_configs, is_from_lock_file) = if let Some(lock_path) = lock_file {
        // Build from lock file
        let lock = rsplug::LockFile::read(&lock_path).await?;
        msg(Message::DetectConfigFile(lock_path));
        
        // TODO: Use lock.plugins[].resolved_rev to enforce exact commits when loading
        // Currently, we parse TOML and load as normal, which may fetch different commits
        // For true deterministic builds, we should match each plugin with its locked
        // revision and pass that to the load function.
        
        // Parse TOML configs from lock file
        let mut configs = Vec::new();
        for toml_config in &lock.toml_configs {
            match toml::from_str::<rsplug::Config>(&toml_config.content) {
                Ok(config) => {
                    configs.push(config);
                }
                Err(e) => return Err(Error::Parse(e, toml_config.path.clone())),
            }
        }
        
        (rsplug::Plugin::new(configs.into_iter().sum())?, lock.toml_configs, true)
    } else {
        // Build from TOML files (existing behavior)
        // Parse all of config files
        let (configs, toml_configs) = {
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
                            // Also save TOML content for lock file
                            let toml_content = String::from_utf8_lossy(&content).to_string();
                            Ok((config, rsplug::TomlConfig {
                                path: path.clone(),
                                content: toml_content,
                            }))
                        }
                        Err(e) => Err(Error::Parse(e, path.to_path_buf())),
                    }
                })
                .collect::<JoinSet<_>>();
            let mut confs = Vec::new();
            let mut toml_confs = Vec::new();
            while let Some(result) = joinset.join_next().await {
                let (config, toml_config) = result.expect(
                    "Some tasks reading config files may be unintentionally aborted",
                )?;
                confs.push(config);
                toml_confs.push(toml_config);
            }
            (confs, toml_confs)
        };
        (rsplug::Plugin::new(configs.into_iter().sum())?, toml_configs, false)
    };

    msg(Message::Loading { install, update });
    // Load plugins through Cache based on the Units
    let (mut plugins, lock_infos) = {
        let res = plugins
            .map(|plugin| plugin.load(install, update, DEFAULT_REPOCACHE_DIR.as_path()))
            .collect::<JoinSet<_>>()
            .join_all()
            .await;
        // Wait until all loading is complete.
        // NOTE: It does not abort if an error occurs (because of the build process).
        msg(Message::LoadDone);
        res.into_iter()
            .try_fold((BinaryHeap::new(), Vec::new()), |(mut plugins, mut locks), res| {
                let result = res?;
                if let Some(loaded_plugin) = result.loaded {
                    plugins.push(loaded_plugin);
                }
                if let Some(lock_info) = result.lock_info {
                    locks.push(lock_info);
                }
                Ok::<_, Error>((plugins, locks))
            })?
    };
    let total_count = plugins.len();

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
    
    // Write lock file only when building from TOML configs (not when using existing lock file)
    if !is_from_lock_file && (install || update) {
        let lock_file = rsplug::LockFile {
            version: "1".to_string(),
            toml_configs,
            plugins: lock_infos.into_iter().map(|info| rsplug::LockedPlugin {
                id: info.id,
                repo: info.repo,
                resolved_rev: info.resolved_rev,
                to_sym: info.to_sym,
                build: info.build,
            }).collect(),
        };
        
        let lock_path = DEFAULT_APP_DIR.join("rsplug.lock.json");
        lock_file.write(&lock_path).await?;
        // TODO: Add proper log message for lock file write
        msg(Message::DetectConfigFile(lock_path));
    }
    
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

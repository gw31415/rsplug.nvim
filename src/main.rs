use std::{collections::BinaryHeap, fmt, path::PathBuf};

use clap::Parser;
use once_cell::sync::Lazy;
use tokio::task::JoinSet;

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
    config_files: Vec<PathBuf>,
}

async fn app() -> Result<(), Error> {
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
                let content = tokio::fs::read_to_string(&path).await.map_err(Error::Io)?;
                let config = toml::from_str::<rsplug::Config>(&content)
                    .map_err(|e| Error::Parse(e, path))?;
                Ok::<_, Error>(config)
            })
            .collect::<JoinSet<_>>()
            .join_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?
            .into_iter();
        rsplug::Unit::new(configs.sum())
    };

    // Fetch packages through Cache based on the Units
    let mut pkgs: BinaryHeap<_> = rsplug::Cache::new(DEFAULT_APP_DIR.as_path())
        .fetch(units, install, update)
        .await?;
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
    Parse(toml::de::Error, PathBuf),
    Io(std::io::Error),
    Rsplug(#[from] rsplug::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use Error::*;
        match self {
            Io(e) => {
                writeln!(f, "{e}")
            }
            Parse(e, file) => {
                writeln!(f, "failed to parse {file:?}:")?;
                write!(f, "{e}")
            }
            Rsplug(e) => {
                panic!("{e}")
            }
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = app().await {
        eprint!("{e}");
        std::process::exit(1);
    }
}

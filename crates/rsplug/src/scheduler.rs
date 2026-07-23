//! Scheduler-owned execution state and repository load phase boundaries.
//!
//! The event loop remains in `main.rs`, while this module owns the validated
//! run mode and the per-plugin EARLY/LATE boundary. Keeping these data models
//! together prevents CLI booleans and phase-specific inputs from leaking into
//! the publication code.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunMode {
    Refresh,
    Install,
    Update,
    InstallAndUpdate,
    Locked,
    LockedInstall,
}

impl RunMode {
    pub(crate) fn from_flags(install: bool, update: bool, locked: bool) -> Self {
        if locked {
            return if install {
                Self::LockedInstall
            } else {
                Self::Locked
            };
        }
        match (install, update) {
            (false, false) => Self::Refresh,
            (true, false) => Self::Install,
            (false, true) => Self::Update,
            (true, true) => Self::InstallAndUpdate,
        }
    }

    pub(crate) fn install(self) -> bool {
        matches!(
            self,
            Self::Install | Self::InstallAndUpdate | Self::LockedInstall
        )
    }

    pub(crate) fn update(self) -> bool {
        matches!(self, Self::Update | Self::InstallAndUpdate)
    }

    pub(crate) fn locked(self) -> bool {
        matches!(self, Self::Locked | Self::LockedInstall)
    }

    pub(crate) fn allows_remote(self) -> bool {
        !self.locked() && (self.install() || self.update())
    }
}

#[derive(Clone)]
pub(crate) struct LoadCtx {
    pub(crate) mode: RunMode,
    pub(crate) locked_map: Arc<BTreeMap<String, rsplug::LockedResource>>,
    pub(crate) network: adaptive_semaphore::NetworkLimits,
    pub(crate) breaker: Arc<rsplug::util::github::CircuitBreaker>,
    pub(crate) http_client: reqwest::Client,
    pub(crate) cache_dir: PathBuf,
    pub(crate) catalogs: Arc<rsplug::RepoJobRegistry>,
}

pub(crate) enum LoadRev {
    Auto,
    Resolved(Option<Arc<str>>),
}

#[allow(clippy::result_large_err)]
fn expand_rev(
    plugin: &rsplug::Plugin,
    ctx: &LoadCtx,
    rev: LoadRev,
) -> Result<(Option<Arc<str>>, Option<String>), Error> {
    Ok(match rev {
        LoadRev::Auto => {
            if let Some(repo) = plugin.cache.repo.as_ref() {
                let canonical = repo.canonical();
                if ctx.mode.locked()
                    && let Some(entry) = ctx.locked_map.get(&canonical)
                {
                    if entry.kind != rsplug::LockedResourceType::Git {
                        return Err(Error::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("Unsupported lock type for {}: {:?}", canonical, entry.kind),
                        )));
                    }
                    (Some(Arc::<str>::from(entry.rev.as_str())), Some(canonical))
                } else if ctx.mode.locked() {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("Missing locked revision for {}", canonical),
                    )));
                } else {
                    (None, Some(canonical))
                }
            } else {
                (None, None)
            }
        }
        LoadRev::Resolved(oid) => {
            let repo_canon = plugin.cache.repo.as_ref().map(|r| r.canonical());
            (oid, repo_canon)
        }
    })
}

pub(crate) async fn run_load_early(
    plugin: &rsplug::Plugin,
    ctx: &LoadCtx,
    rev: LoadRev,
) -> Result<(rsplug::EarlyOutcome, Option<String>), Error> {
    let (locked_rev, repo_canon) = expand_rev(plugin, ctx, rev)?;
    let early = plugin
        .load_early(
            ctx.mode.install(),
            ctx.mode.update(),
            &ctx.cache_dir,
            locked_rev.as_deref(),
            &ctx.network,
            &ctx.breaker,
            &ctx.http_client,
            &ctx.catalogs,
        )
        .await?;
    Ok((early, repo_canon))
}

pub(crate) async fn run_load_late(
    plugin: rsplug::Plugin,
    early: rsplug::EarlyOutcome,
    ctx: &LoadCtx,
) -> Result<
    (
        Option<(rsplug::LoadedPlugin, Option<(String, String)>)>,
        Option<String>,
    ),
    Error,
> {
    let repo_canon = plugin.cache.repo.as_ref().map(|r| r.canonical());
    let result = plugin
        .load_late(early, &ctx.cache_dir, ctx.mode.update(), &ctx.catalogs)
        .await;
    msg(Message::LoadPluginDone);
    let canon_to_remove =
        if repo_canon.is_some() && result.is_ok() && result.as_ref().unwrap().is_none() {
            repo_canon
        } else {
            None
        };
    Ok((result?, canon_to_remove))
}

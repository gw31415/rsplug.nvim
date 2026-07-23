//! Repository object acquisition and snapshot materialization.
//!
//! Resolution decides *which* OID is wanted; this module owns the bounded
//! source.git/tarball acquisition and immutable snapshot destination.

use super::*;

pub(super) async fn ensure_source_git(ctx: &FetchCtx<'_>) -> Result<bool, Error> {
    let cell = ctx
        .jobs
        .acquisition_cell(ctx.source_git, ctx.oid, ctx.dotgit)
        .await;
    let result = cell
        .get_or_init(|| async {
            ensure_source_git_inner(ctx)
                .await
                .map_err(|error| Arc::from(error.to_string()))
        })
        .await;
    result
        .clone()
        .map_err(|message| Error::Io(std::io::Error::other(message.as_ref())))
}

async fn ensure_source_git_inner(ctx: &FetchCtx<'_>) -> Result<bool, Error> {
    use super::util::git;
    use crate::log::{Message, msg};

    let source_lock = ctx.jobs.source_git_lock(ctx.source_git);
    let _source_guard = source_lock.lock().await;
    let mut repo = match git::open_source(ctx.source_git).await {
        Ok(r) => r,
        Err(_) if ctx.install || ctx.update => {
            let _git = super::util::resources::git().await?;
            msg(Message::Cache("Initializing", ctx.url.clone()));
            git::init_source(ctx.source_git, ctx.url).await?
        }
        Err(_) if ctx.locked => {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Missing cached repository for locked revision: {}", ctx.url),
            )));
        }
        Err(_) => {
            msg(Message::PluginNotInstalled(display_name(
                ctx.source_name,
                ctx.logid,
            )));
            return Ok(false);
        }
    };
    if repo.contains_oid(ctx.oid).await? {
        return Ok(true);
    }
    let _git = super::util::resources::git().await?;
    msg(Message::Cache("Fetching", ctx.url.clone()));
    crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::GitFetch);
    let host = util::repo::host_of(ctx.url);
    ctx.network
        .run(&host, repo.fetch_oid(ctx.oid, ctx.token.clone()))
        .await?;
    msg(Message::Cache("Fetching:done", ctx.url.clone()));
    Ok(true)
}

pub(super) async fn materialize(
    ctx: &FetchCtx<'_>,
    dest: &Path,
    use_tarball: bool,
) -> Result<Option<MaterializedRepo>, Error> {
    use super::util::git;

    let key = MaterializationKey {
        canonical: ctx.canonical.to_owned(),
        oid: ctx.oid.to_string(),
        use_tarball,
        dotgit: ctx.dotgit,
        destination: dest.to_path_buf(),
    };
    let cell = ctx.jobs.materialization_cell(key).await;
    if cell.get().is_some() {
        crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::DuplicateMaterializeJob);
    }
    let result = cell
        .get_or_init(|| async {
            materialize_inner(ctx, dest, use_tarball)
                .await
                .map_err(|error| Arc::<str>::from(error.to_string()))
        })
        .await;
    let snapshot = result
        .clone()
        .map_err(|message| Error::Io(std::io::Error::other(message.as_ref())))?;
    let Some(snapshot) = snapshot else {
        return Ok(None);
    };
    if snapshot.plain {
        Ok(Some(MaterializedRepo::Plain))
    } else {
        Ok(Some(MaterializedRepo::Git(git::open(snapshot.root).await?)))
    }
}

async fn materialize_inner(
    ctx: &FetchCtx<'_>,
    dest: &Path,
    use_tarball: bool,
) -> Result<Option<MaterializedSnapshot>, Error> {
    use super::util::{fetch::TarballFetch, git};
    use crate::log::{Message, msg};

    let _materialize_guard = ctx.jobs.materialize_lock(dest).lock_owned().await;
    if tokio::fs::try_exists(dest).await.unwrap_or(false) {
        crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::DuplicateMaterializeJob);
        let plain = !tokio::fs::try_exists(dest.join(".git"))
            .await
            .unwrap_or(false);
        return Ok(Some(MaterializedSnapshot {
            root: Arc::from(dest.to_path_buf()),
            plain,
        }));
    }

    if use_tarball {
        crate::rsplug::perf::failpoint("materialize_before")?;
        let tarball_ok = {
            msg(Message::Cache("Fetching", ctx.url.clone()));
            let head_rev = ctx.oid.to_string();
            crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::TarballFetch);
            let download = ctx
                .network
                .run(
                    "codeload.github.com",
                    TarballFetch.download(
                        ctx.http_client,
                        ctx.url.as_ref(),
                        &head_rev,
                        dest,
                        ctx.token.as_deref(),
                    ),
                )
                .await;
            let ok = match download {
                Ok(archive) => archive.extract_to_snapshot(dest).await.is_ok(),
                Err(_) => false,
            };
            if ok {
                msg(Message::Cache("Fetching:done", ctx.url.clone()));
            }
            crate::rsplug::perf::incr(if ok {
                crate::rsplug::perf::PerfOp::PermitSuccess
            } else {
                crate::rsplug::perf::PerfOp::PermitError
            });
            ok
        };
        if tarball_ok {
            return Ok(Some(MaterializedSnapshot {
                root: Arc::from(dest.to_path_buf()),
                plain: true,
            }));
        }
        if !ensure_source_git(ctx).await? {
            return Ok(None);
        }
    }

    let _git = super::util::resources::git().await?;
    crate::rsplug::perf::failpoint("materialize_after")?;
    git::init_snapshot(dest.to_path_buf(), ctx.source_git, ctx.oid).await?;
    Ok(Some(MaterializedSnapshot {
        root: Arc::from(dest.to_path_buf()),
        plain: false,
    }))
}

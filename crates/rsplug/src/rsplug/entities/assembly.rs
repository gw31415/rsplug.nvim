//! Plugin assembly boundary.
//!
//! The implementation remains deliberately thin because the parent module
//! owns the repository-specific types; this module is the stable boundary for
//! the snapshot-to-`LoadedPlugin` phase.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn assemble_loaded_plugin(
    snapshot_root_path: &Arc<Path>,
    identity: &RepoSnapshotIdentity,
    catalogs: &SnapshotCatalogCache,
    dotgit: bool,
    merge: &MergeConfig,
    lazy_type: LazyType,
    source_name: Option<String>,
    script: SetupScript,
    order: usize,
    merge_enabled: bool,
    was_updated: bool,
    was_installed: bool,
    logid: &str,
) -> Result<LoadedPlugin, Error> {
    super::assemble_loaded_plugin(
        snapshot_root_path,
        identity,
        catalogs,
        dotgit,
        merge,
        lazy_type,
        source_name,
        script,
        order,
        merge_enabled,
        was_updated,
        was_installed,
        logid,
    )
    .await
}

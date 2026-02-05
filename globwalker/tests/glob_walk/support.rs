use globwalker::GlobWalker;
use std::collections::HashSet;
use std::fs as stdfs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
pub(crate) use std::os::unix::fs::symlink;

#[cfg(all(unix, target_os = "linux"))]
pub(crate) use std::ffi::OsString;
#[cfg(all(unix, target_os = "linux"))]
pub(crate) use std::os::unix::ffi::OsStringExt;

static NEXT_TEST_DIR_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) struct TestDir {
    pub(crate) path: PathBuf,
}

impl TestDir {
    pub(crate) fn create() -> io::Result<Self> {
        let temp_dir = std::env::temp_dir();
        let process_id = std::process::id();

        for _ in 0..1024 {
            let unique_id = NEXT_TEST_DIR_ID.fetch_add(1, Ordering::Relaxed);
            let path = temp_dir.join(format!("globwalker-test-{process_id}-{unique_id}"));

            match stdfs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }

        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Unable to allocate a unique test directory name",
        ))
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = stdfs::remove_dir_all(&self.path);
    }
}

pub(crate) fn create_file(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        stdfs::create_dir_all(parent)?;
    }
    stdfs::write(path, "test")?;
    Ok(())
}

pub(crate) async fn collect_paths(mut walker: GlobWalker) -> io::Result<Vec<String>> {
    let mut result = Vec::new();
    while let Some(path) = walker.next().await? {
        result.push(path);
    }
    Ok(result)
}

pub(crate) fn collect_set(paths: &[String]) -> HashSet<&str> {
    paths.iter().map(|path| path.as_ref()).collect()
}

#[cfg(unix)]
pub(crate) fn deny_permissions(path: &Path) -> io::Result<stdfs::Permissions> {
    let original_permissions = stdfs::metadata(path)?.permissions();
    stdfs::set_permissions(path, stdfs::Permissions::from_mode(0o000))?;
    Ok(original_permissions)
}

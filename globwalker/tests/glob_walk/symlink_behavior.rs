use std::io;

use globwalker::GlobWalker;

use super::support::{TestDir, collect_paths, collect_set, create_file};

#[tokio::test]
async fn deduplicates_by_canonical_path_and_ignores_broken_symlink() -> io::Result<()> {
    let test_dir = TestDir::create()?;
    let real_file = test_dir.path.join("real/target.txt");
    create_file(&real_file)?;

    #[cfg(unix)]
    {
        let alias_file = test_dir.path.join("real/alias.txt");
        super::support::symlink(&real_file, &alias_file)?;
        super::support::symlink(
            test_dir.path.join("real/missing.txt"),
            test_dir.path.join("broken.txt"),
        )?;

        let walker = GlobWalker::new(vec!["**/*.txt".to_string()], &test_dir.path)?;
        let result = collect_paths(walker).await?;
        assert_eq!(result.len(), 1);
        return Ok(());
    }

    #[cfg(not(unix))]
    {
        let walker = GlobWalker::new(vec!["**/*.txt".to_string()], &test_dir.path)?;
        let result = collect_paths(walker).await?;
        assert_eq!(result.len(), 1);
        Ok(())
    }
}

#[tokio::test]
#[cfg(unix)]
async fn follows_symlinked_directories_outside_root() -> io::Result<()> {
    let root = TestDir::create()?;
    let outside = TestDir::create()?;
    create_file(&root.path.join("inside/ok.txt"))?;
    create_file(&outside.path.join("secret/hidden.txt"))?;
    super::support::symlink(&outside.path, root.path.join("escape"))?;

    let walker = GlobWalker::new(vec!["**/*.txt".to_string()], &root.path)?;
    let result = collect_paths(walker).await?;

    let paths = collect_set(&result);
    assert!(paths.contains("inside/ok.txt"));
    assert!(paths.contains("escape/secret/hidden.txt"));
    Ok(())
}

#[tokio::test]
#[cfg(all(unix, target_os = "linux"))]
async fn includes_non_utf8_entries_without_failing_scan() -> io::Result<()> {
    let test_dir = TestDir::create()?;
    create_file(&test_dir.path.join("inside/ok.txt"))?;

    let non_utf8_name = super::support::OsString::from_vec(vec![0xff, b'.', b't', b'x', b't']);
    let non_utf8_path = test_dir.path.join(non_utf8_name);
    std::fs::write(non_utf8_path, "test")?;

    let walker = GlobWalker::new(vec!["**/*.txt".to_string()], &test_dir.path)?;
    let result = collect_paths(walker).await?;

    let paths = collect_set(&result);
    assert_eq!(paths.len(), 2);
    assert!(paths.contains("inside/ok.txt"));
    assert!(paths.iter().any(|path| *path != "inside/ok.txt"));
    Ok(())
}

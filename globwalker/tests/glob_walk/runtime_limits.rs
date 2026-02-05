use std::io;
use std::time::Instant;

use globwalker::GlobWalker;

use super::support::{TestDir, collect_paths, create_file};

#[tokio::test]
#[cfg(unix)]
async fn streams_first_result_before_error_from_later_scan() -> io::Result<()> {
    let test_dir = TestDir::create()?;
    create_file(&test_dir.path.join("first.txt"))?;
    create_file(&test_dir.path.join("blocked/hidden.txt"))?;

    let blocked_dir = test_dir.path.join("blocked");
    let original_permissions = super::support::deny_permissions(&blocked_dir)?;

    let mut walker = GlobWalker::new(
        vec!["*.txt".to_string(), "blocked/**/*.txt".to_string()],
        &test_dir.path,
    ).await?;
    let first = walker
        .next()
        .await?
        .ok_or_else(|| io::Error::other("Expected the first streamed result"))?;
    assert_eq!(first, "first.txt");

    let second_result = walker.next().await;
    std::fs::set_permissions(&blocked_dir, original_permissions)?;

    assert!(matches!(
        second_result,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied
    ));
    Ok(())
}

#[tokio::test]
async fn rejects_too_many_patterns() -> io::Result<()> {
    let test_dir = TestDir::create()?;
    let patterns = (0..4097)
        .map(|_| "**/*.txt".to_string())
        .collect::<Vec<_>>();

    let result = GlobWalker::new(patterns, &test_dir.path).await;

    assert!(matches!(
        result,
        Err(error) if error.kind() == io::ErrorKind::InvalidInput
    ));
    Ok(())
}

#[tokio::test]
async fn rejects_too_long_pattern() -> io::Result<()> {
    let test_dir = TestDir::create()?;
    let long_pattern = "a".repeat(4097);

    let result = GlobWalker::new(vec![long_pattern], &test_dir.path).await;

    assert!(matches!(
        result,
        Err(error) if error.kind() == io::ErrorKind::InvalidInput
    ));
    Ok(())
}

#[tokio::test]
async fn allows_parent_directory_traversal_pattern() -> io::Result<()> {
    let root = TestDir::create()?;
    let outside = TestDir::create()?;
    create_file(&outside.path.join("external/file.txt"))?;

    let pattern = format!(
        "../{}/**/*.txt",
        outside.path.file_name().unwrap().to_string_lossy()
    );
    let walker = GlobWalker::new(vec![pattern], &root.path).await?;
    let result = collect_paths(walker).await?;

    assert!(
        result
            .iter()
            .any(|path| path.ends_with("/external/file.txt"))
    );
    Ok(())
}

#[tokio::test]
async fn supports_bare_parent_descends_pattern() -> io::Result<()> {
    let base = TestDir::create()?;
    let root = base.path.join("root");
    std::fs::create_dir_all(&root)?;
    create_file(&base.path.join("outside/file.txt"))?;

    let walker = GlobWalker::new(vec!["../**/*.txt".to_string()], &root).await?;
    let result = collect_paths(walker).await?;

    assert!(
        result
            .iter()
            .any(|path| path.ends_with("/outside/file.txt"))
    );
    Ok(())
}

#[tokio::test]
async fn returns_timeout_error_after_deadline_is_reached() -> io::Result<()> {
    let test_dir = TestDir::create()?;
    create_file(&test_dir.path.join("a/file.txt"))?;

    let mut walker = GlobWalker::new(vec!["**/*.txt".to_string()], &test_dir.path).await?;
    walker.set_deadline(Instant::now());

    let result = walker.next().await;
    assert!(matches!(
        result,
        Err(error) if error.kind() == io::ErrorKind::TimedOut
    ));
    Ok(())
}

#[tokio::test]
async fn root_wide_pattern_still_collects_files() -> io::Result<()> {
    let test_dir = TestDir::create()?;
    create_file(&test_dir.path.join("a/one.txt"))?;
    create_file(&test_dir.path.join("b/two.txt"))?;

    let walker = GlobWalker::new(vec!["**/*.txt".to_string()], &test_dir.path).await?;
    let results = collect_paths(walker).await?;

    assert_eq!(results.len(), 2);
    Ok(())
}

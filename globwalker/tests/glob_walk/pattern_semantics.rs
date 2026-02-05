use std::io;

use globwalker::GlobWalker;

use super::support::{TestDir, collect_paths, collect_set, create_file};

#[tokio::test]
async fn prunes_unrelated_directory_and_avoids_error() -> io::Result<()> {
    let test_dir = TestDir::create()?;
    create_file(&test_dir.path.join("hoge/fuga/aoeu/matched.txt"))?;
    std::fs::create_dir_all(test_dir.path.join("hoge/hito"))?;

    #[cfg(unix)]
    {
        let denied_path = test_dir.path.join("hoge/hito");
        let original_permissions = super::support::deny_permissions(&denied_path)?;

        let walker = GlobWalker::new(vec!["hoge/fuga/**/*.txt".to_string()], &test_dir.path).await;
        std::fs::set_permissions(&denied_path, original_permissions)?;
        let result = collect_paths(walker?).await?;

        assert!(
            result
                .iter()
                .any(|path| path == "hoge/fuga/aoeu/matched.txt")
        );
        return Ok(());
    }

    #[cfg(not(unix))]
    {
        let walker =
            GlobWalker::new(vec!["hoge/fuga/**/*.txt".to_string()], &test_dir.path).await?;
        let result = collect_paths(walker).await?;
        assert!(
            result
                .iter()
                .any(|path| path.as_ref() == "hoge/fuga/aoeu/matched.txt")
        );
        Ok(())
    }
}

#[tokio::test]
async fn applies_last_match_wins_with_excludes() -> io::Result<()> {
    let test_dir = TestDir::create()?;
    create_file(&test_dir.path.join("target/keep.txt"))?;
    create_file(&test_dir.path.join("target/ignore.txt"))?;

    let walker = GlobWalker::new(
        vec![
            "**/*.txt".to_string(),
            "!**/ignore.txt".to_string(),
            "**/ignore.txt".to_string(),
        ],
        &test_dir.path,
    ).await?;
    let result = collect_paths(walker).await?;

    let paths = collect_set(&result);
    assert!(paths.contains("target/keep.txt"));
    assert!(paths.contains("target/ignore.txt"));
    Ok(())
}

#[tokio::test]
async fn normalizes_pattern_prefix_and_double_star_dot() -> io::Result<()> {
    let test_dir = TestDir::create()?;
    create_file(&test_dir.path.join("a/b/sample.txt"))?;
    create_file(&test_dir.path.join("a/b/skip.md"))?;

    let walker = GlobWalker::new(vec!["./**.txt".to_string()], &test_dir.path).await?;
    let result = collect_paths(walker).await?;

    let paths = collect_set(&result);
    assert_eq!(paths.len(), 1);
    assert!(paths.contains("a/b/sample.txt"));
    Ok(())
}

#[tokio::test]
async fn normalizes_double_star_to_recursive_match() -> io::Result<()> {
    let test_dir = TestDir::create()?;
    create_file(&test_dir.path.join("root.txt"))?;
    create_file(&test_dir.path.join("a/one.txt"))?;
    create_file(&test_dir.path.join("a/b/two.txt"))?;

    let walker = GlobWalker::new(vec!["**".to_string()], &test_dir.path).await?;
    let result = collect_paths(walker).await?;

    let paths = collect_set(&result);
    assert_eq!(paths.len(), 3);
    assert!(paths.contains("root.txt"));
    assert!(paths.contains("a/one.txt"));
    assert!(paths.contains("a/b/two.txt"));
    Ok(())
}

#[tokio::test]
async fn single_star_does_not_cross_directory_boundaries() -> io::Result<()> {
    let test_dir = TestDir::create()?;
    create_file(&test_dir.path.join(".config/home-manager/base.toml"))?;
    create_file(
        &test_dir
            .path
            .join(".config/home-manager/nvim/rsplug/nested.toml"),
    )?;

    let walker = GlobWalker::new(
        vec![".config/home-manager/*.toml".to_string()],
        &test_dir.path,
    ).await?;
    let result = collect_paths(walker).await?;

    let paths = collect_set(&result);
    assert_eq!(paths.len(), 1);
    assert!(paths.contains(".config/home-manager/base.toml"));
    Ok(())
}

#[tokio::test]
async fn supports_absolute_patterns_within_root() -> io::Result<()> {
    let test_dir = TestDir::create()?;
    create_file(&test_dir.path.join("a/b/sample.txt"))?;
    create_file(&test_dir.path.join("a/b/skip.md"))?;

    let absolute_pattern = test_dir
        .path
        .join("a/**/*.txt")
        .to_string_lossy()
        .to_string();
    let walker = GlobWalker::new(vec![absolute_pattern], &test_dir.path).await?;
    let result = collect_paths(walker).await?;

    let paths = collect_set(&result);
    assert_eq!(paths.len(), 1);
    assert!(paths.contains("a/b/sample.txt"));
    Ok(())
}

#[tokio::test]
async fn supports_absolute_patterns_outside_root() -> io::Result<()> {
    let root = TestDir::create()?;
    let outside = TestDir::create()?;
    create_file(&outside.path.join("a/b/sample.txt"))?;

    let outside_pattern = outside
        .path
        .join("a/**/*.txt")
        .to_string_lossy()
        .to_string();
    let walker = GlobWalker::new(vec![outside_pattern], &root.path).await?;
    let result = collect_paths(walker).await?;

    assert!(result.iter().any(|path| path.ends_with("/a/b/sample.txt")));
    Ok(())
}

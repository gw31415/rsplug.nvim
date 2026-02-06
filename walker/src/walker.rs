use crate::compiled_glob::CompiledGlob;
use std::fmt;
use std::io;
use std::path::PathBuf;
use tokio::sync::mpsc;

#[cfg(not(windows))]
#[path = "walker_unix.rs"]
mod backend;
#[cfg(windows)]
#[path = "walker_windows.rs"]
mod backend;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug)]
pub struct WalkEvent {
    pub path: PathBuf,
    pub kind: EntryKind,
}

#[derive(Debug)]
pub enum WalkError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Unsupported {
        feature: &'static str,
        path: PathBuf,
    },
}

impl fmt::Display for WalkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalkError::Io { path, source } => {
                write!(f, "io error at {}: {source}", path.display())
            }
            WalkError::Unsupported { feature, path } => {
                write!(f, "unsupported feature `{feature}` at {}", path.display())
            }
        }
    }
}

impl std::error::Error for WalkError {}

pub type WalkMessage = Result<WalkEvent, WalkError>;

#[derive(Clone, Debug)]
pub struct WalkerOptions {
    pub max_parallelism: Option<usize>,
    pub channel_capacity: usize,
    pub files_only: bool,
}

impl Default for WalkerOptions {
    fn default() -> Self {
        Self {
            max_parallelism: None,
            channel_capacity: 1024,
            files_only: false,
        }
    }
}

pub struct Walker;

impl Walker {
    pub fn spawn(compiled: CompiledGlob) -> mpsc::Receiver<WalkMessage> {
        Self::spawn_with_options(compiled, WalkerOptions::default())
    }

    pub fn spawn_many(
        globs: impl IntoIterator<Item = CompiledGlob>,
    ) -> mpsc::Receiver<WalkMessage> {
        Self::spawn_many_with_options(globs, WalkerOptions::default())
    }

    pub fn spawn_with_options(
        compiled: CompiledGlob,
        options: WalkerOptions,
    ) -> mpsc::Receiver<WalkMessage> {
        Self::spawn_many_with_options([compiled], options)
    }

    pub fn spawn_many_with_options(
        globs: impl IntoIterator<Item = CompiledGlob>,
        options: WalkerOptions,
    ) -> mpsc::Receiver<WalkMessage> {
        let merged = match CompiledGlob::merge_many(globs) {
            Ok(merged) => merged,
            Err(err) => {
                let (tx, rx) = mpsc::channel(options.channel_capacity.max(1));
                tokio::spawn(async move {
                    let _ = tx
                        .send(Err(WalkError::Io {
                            path: PathBuf::from("<spawn_many>"),
                            source: err,
                        }))
                        .await;
                });
                return rx;
            }
        };

        backend::spawn_single_with_options(merged, options)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(all(unix, not(windows)))]
    use super::*;
    use crate::compiled_glob::CompiledGlob;
    #[cfg(all(unix, not(windows)))]
    use std::collections::BTreeSet;
    #[cfg(all(unix, not(windows)))]
    use std::fs;
    #[cfg(all(unix, not(windows)))]
    use std::path::PathBuf;
    #[cfg(all(unix, not(windows)))]
    use std::time::Duration;
    #[cfg(all(unix, not(windows)))]
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(all(unix, not(windows)))]
    fn test_root(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        std::env::temp_dir().join(format!("walker-{name}-{stamp}"))
    }

    #[tokio::test]
    async fn descend_match_equivalence() {
        let one = CompiledGlob::new("a/**/**/b").expect("glob must parse");
        let two = CompiledGlob::new("a/**/b").expect("glob must parse");
        for path in ["a/b", "a/x/b", "a/x/y/b", "a/x/y/c", "x/a/b", "a/x/y/z"] {
            assert_eq!(one.r#match(path.as_ref()), two.r#match(path.as_ref()));
        }
    }

    #[tokio::test]
    #[cfg(all(unix, not(windows)))]
    async fn streams_results_before_full_walk() {
        let root = test_root("stream");
        fs::create_dir_all(root.join("d1/d2")).expect("create tree");
        fs::write(root.join("d1/a.txt"), b"a").expect("write file");
        fs::write(root.join("d1/d2/b.txt"), b"b").expect("write file");

        let pattern = format!("{}/**", root.display());
        let glob = CompiledGlob::new(&pattern).expect("glob must parse");
        let mut rx = Walker::spawn(glob);

        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("must receive quickly");
        assert!(first.is_some());

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[cfg(all(unix, not(windows)))]
    async fn finds_expected_paths() {
        let root = test_root("paths");
        fs::create_dir_all(root.join("src/bin")).expect("create tree");
        fs::create_dir_all(root.join("docs")).expect("create tree");
        fs::write(root.join("src/main.rs"), b"fn main(){}").expect("write file");
        fs::write(root.join("src/bin/tool.rs"), b"fn main(){}").expect("write file");
        fs::write(root.join("docs/readme.md"), b"# hi").expect("write file");

        let pattern = format!("{}/**/*.rs", root.display());
        let glob = CompiledGlob::new(&pattern).expect("glob must parse");
        let mut rx = Walker::spawn(glob);

        let mut got = BTreeSet::new();
        while let Some(msg) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("channel should respond")
        {
            if let Ok(ev) = msg {
                got.insert(
                    ev.path
                        .strip_prefix(&root)
                        .expect("path under root")
                        .to_path_buf(),
                );
            }
        }

        let expected: BTreeSet<PathBuf> = ["src/main.rs", "src/bin/tool.rs"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(got, expected);
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[cfg(all(unix, not(windows)))]
    async fn symlink_loop_does_not_hang() {
        use std::os::unix::fs::symlink;

        let root = test_root("loop");
        fs::create_dir_all(root.join("a/b")).expect("create tree");
        fs::write(root.join("a/b/file.txt"), b"1").expect("write file");
        symlink(root.join("a"), root.join("a/b/link_to_a")).expect("create symlink");

        let pattern = format!("{}/**", root.display());
        let glob = CompiledGlob::new(&pattern).expect("glob must parse");
        let mut rx = Walker::spawn(glob);

        let mut count = 0usize;
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
            count += 1;
            if count > 64 {
                break;
            }
        }
        assert!(count > 0);
        assert!(count <= 64);

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[cfg(all(unix, not(windows)))]
    async fn permission_denied_does_not_abort_descend_walk() {
        use std::os::unix::fs::PermissionsExt;

        let root = test_root("perm");
        fs::create_dir_all(root.join("ok")).expect("create tree");
        fs::create_dir_all(root.join("blocked")).expect("create tree");
        fs::write(root.join("ok/keep.rs"), b"fn main(){}").expect("write file");
        fs::set_permissions(root.join("blocked"), fs::Permissions::from_mode(0o0))
            .expect("chmod blocked");

        let pattern = format!("{}/**.rs", root.display());
        let glob = CompiledGlob::new(&pattern).expect("glob must parse");
        let mut rx = Walker::spawn(glob);

        let mut got_ok = false;
        while let Some(msg) = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("channel should respond")
        {
            match msg {
                Ok(ev) => {
                    if ev.path == root.join("ok/keep.rs") {
                        got_ok = true;
                    }
                }
                Err(WalkError::Io { .. }) => {}
                Err(WalkError::Unsupported { .. }) => {}
            }
        }

        assert!(got_ok, "accessible matches should still be emitted");

        fs::set_permissions(root.join("blocked"), fs::Permissions::from_mode(0o755))
            .expect("restore perms");
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[cfg(all(unix, not(windows)))]
    async fn spawn_many_matches_union_of_patterns() {
        let root = test_root("many_union");
        fs::create_dir_all(root.join("src")).expect("create tree");
        fs::create_dir_all(root.join("docs")).expect("create tree");
        fs::write(root.join("src/main.rs"), b"fn main(){}").expect("write file");
        fs::write(root.join("docs/readme.md"), b"# hi").expect("write file");

        let g1 =
            CompiledGlob::new(&format!("{}/**/*.rs", root.display())).expect("glob must parse");
        let g2 =
            CompiledGlob::new(&format!("{}/**/*.md", root.display())).expect("glob must parse");
        let mut rx = Walker::spawn_many(vec![g1, g2]);

        let mut got = BTreeSet::new();
        while let Some(msg) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("channel should respond")
        {
            if let Ok(ev) = msg {
                got.insert(
                    ev.path
                        .strip_prefix(&root)
                        .expect("path under root")
                        .to_path_buf(),
                );
            }
        }

        let expected: BTreeSet<PathBuf> = ["src/main.rs", "docs/readme.md"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(got, expected);
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[cfg(all(unix, not(windows)))]
    async fn spawn_many_single_equivalent_to_spawn() {
        let root = test_root("many_single");
        fs::create_dir_all(root.join("src/bin")).expect("create tree");
        fs::write(root.join("src/main.rs"), b"fn main(){}").expect("write file");
        fs::write(root.join("src/bin/tool.rs"), b"fn main(){}").expect("write file");

        let pattern = format!("{}/**/*.rs", root.display());
        let glob = CompiledGlob::new(&pattern).expect("glob must parse");

        let mut single_rx = Walker::spawn(CompiledGlob::new(&pattern).expect("glob must parse"));
        let mut many_rx = Walker::spawn_many(vec![glob]);

        let mut single = BTreeSet::new();
        while let Some(msg) = tokio::time::timeout(Duration::from_secs(2), single_rx.recv())
            .await
            .expect("channel should respond")
        {
            if let Ok(ev) = msg {
                single.insert(
                    ev.path
                        .strip_prefix(&root)
                        .expect("path under root")
                        .to_path_buf(),
                );
            }
        }

        let mut many = BTreeSet::new();
        while let Some(msg) = tokio::time::timeout(Duration::from_secs(2), many_rx.recv())
            .await
            .expect("channel should respond")
        {
            if let Ok(ev) = msg {
                many.insert(
                    ev.path
                        .strip_prefix(&root)
                        .expect("path under root")
                        .to_path_buf(),
                );
            }
        }

        assert_eq!(single, many);
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[cfg(all(unix, not(windows)))]
    async fn spawn_many_duplicate_patterns_no_duplicate_path() {
        let root = test_root("many_dup");
        fs::create_dir_all(root.join("src")).expect("create tree");
        fs::write(root.join("src/main.rs"), b"fn main(){}").expect("write file");

        let pattern = format!("{}/**/*.rs", root.display());
        let g1 = CompiledGlob::new(&pattern).expect("glob must parse");
        let g2 = CompiledGlob::new(&pattern).expect("glob must parse");
        let mut rx = Walker::spawn_many(vec![g1, g2]);

        let mut count = 0usize;
        while let Some(msg) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("channel should respond")
        {
            if let Ok(ev) = msg
                && ev.path.ends_with("src/main.rs")
            {
                count += 1;
            }
        }

        assert_eq!(count, 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[cfg(all(unix, not(windows)))]
    async fn spawn_many_applies_last_match_wins_with_excludes() {
        let root = test_root("many_exclude");
        fs::create_dir_all(root.join("target")).expect("create tree");
        fs::write(root.join("target/keep.txt"), b"x").expect("write file");
        fs::write(root.join("target/ignore.txt"), b"x").expect("write file");

        let include =
            CompiledGlob::new(&format!("{}/**/*.txt", root.display())).expect("glob must parse");
        let exclude = CompiledGlob::new(&format!("!{}/**/ignore.txt", root.display()))
            .expect("glob must parse");
        let reinclude = CompiledGlob::new(&format!("{}/**/ignore.txt", root.display()))
            .expect("glob must parse");
        let mut rx = Walker::spawn_many(vec![include, exclude, reinclude]);

        let mut got = BTreeSet::new();
        while let Some(msg) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("channel should respond")
        {
            if let Ok(ev) = msg {
                got.insert(
                    ev.path
                        .strip_prefix(&root)
                        .expect("path under root")
                        .to_path_buf(),
                );
            }
        }

        let expected: BTreeSet<PathBuf> = ["target/keep.txt", "target/ignore.txt"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(got, expected);
        let _ = fs::remove_dir_all(&root);
    }
}

use std::{borrow::Cow, convert::Infallible, path::Path, str::FromStr};

use wildmatch::WildMatch;

/// Gitignore形式のファイル指定子
pub struct FileSpecifier(Vec<FileSpecifierPattern>, String);

#[derive(Debug)]
struct FileSpecifierPattern {
    matcher: WildMatch,
    matcher_for_any_depth: Option<WildMatch>,
    path_only: bool,
    negated: bool,
}

impl std::fmt::Debug for FileSpecifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("FileSpecifier").field(&self.1).finish()
    }
}

impl FileSpecifier {
    pub fn matched(&self, filepath: impl AsRef<Path>) -> bool {
        let path = filepath.as_ref().to_string_lossy();
        let path = if path.contains('\\') {
            Cow::Owned(path.replace('\\', "/"))
        } else {
            path
        };
        let mut ignored = false;

        for pat in &self.0 {
            let matches = if pat.path_only {
                pat.matcher.matches(&path)
                    || pat
                        .matcher_for_any_depth
                        .as_ref()
                        .is_some_and(|m| m.matches(&path))
            } else {
                path.split('/').any(|seg| pat.matcher.matches(seg))
            };

            if matches {
                ignored = !pat.negated;
            }
        }

        ignored
    }
}

impl FromStr for FileSpecifier {
    type Err = Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut patterns = Vec::new();
        for line in s.lines() {
            let mut line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let negated = line.starts_with('!');
            if negated {
                line = line.strip_prefix('!').unwrap_or(line);
            }
            if let Some(rest) = line.strip_prefix(r"\!") {
                line = rest;
            } else if let Some(rest) = line.strip_prefix(r"\#") {
                line = rest;
            }
            if line.is_empty() {
                continue;
            }

            let anchored_to_root = line.starts_with('/');
            let line = line.trim_start_matches('/');
            let had_slash = line.contains('/');
            let directory_only = line.ends_with('/');
            let line = line.trim_end_matches('/');
            if line.is_empty() {
                continue;
            }

            let path_only = had_slash || directory_only;
            let body = if directory_only {
                format!("{line}/**")
            } else {
                line.to_string()
            };
            let matcher_for_any_depth = if path_only && !anchored_to_root {
                Some(WildMatch::new(&format!("**/{body}")))
            } else {
                None
            };

            patterns.push(FileSpecifierPattern {
                matcher: WildMatch::new(&body),
                matcher_for_any_depth,
                path_only,
                negated,
            });
        }
        Ok(FileSpecifier(patterns, s.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::FileSpecifier;
    use std::{path::Path, str::FromStr};

    #[test]
    fn file_specifier_matches_by_segment() {
        let spec = FileSpecifier::from_str("README.md\nLICENSE*").expect("must parse");

        assert!(spec.matched(Path::new("plugin/README.md")));
        assert!(spec.matched(Path::new("foo/LICENSE.txt")));
        assert!(!spec.matched(Path::new("plugin/main.lua")));
    }

    #[test]
    fn file_specifier_supports_directory_pattern() {
        let spec = FileSpecifier::from_str("tests/").expect("must parse");

        assert!(spec.matched(Path::new("tests/a.lua")));
        assert!(spec.matched(Path::new("foo/tests/b.lua")));
        assert!(!spec.matched(Path::new("test/a.lua")));
    }

    #[test]
    fn file_specifier_supports_negation() {
        let spec = FileSpecifier::from_str("*.md\n!README.md").expect("must parse");

        assert!(spec.matched(Path::new("docs/guide.md")));
        assert!(!spec.matched(Path::new("README.md")));
    }

    #[test]
    fn file_specifier_supports_root_anchored_path() {
        let spec = FileSpecifier::from_str("/doc/*.txt").expect("must parse");

        assert!(spec.matched(Path::new("doc/help.txt")));
        assert!(!spec.matched(Path::new("plugin/doc/help.txt")));
    }
}

use path_dedot::{CWD, ParseDot};
use std::ffi::OsStr;
use std::fmt::Debug;
use std::io;
use std::ops::Range;
use std::path::{MAIN_SEPARATOR, Path};
use std::sync::Arc;
use wildmatch::WildMatch;

pub(crate) struct PathInner {
    pathbase: Arc<String>,
    range: Range<usize>,
}

impl PathInner {
    pub(crate) fn as_str(&self) -> &str {
        &self.pathbase[self.range.clone()]
    }
}

impl Debug for PathInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.as_ref())
    }
}

impl AsRef<Path> for PathInner {
    fn as_ref(&self) -> &Path {
        Path::new(&self.pathbase[self.range.clone()])
    }
}

#[derive(Debug)]
pub(crate) enum SegmentMatcher {
    AnyPath(PathInner),
    WildMatch(WildMatch),
    Descend,
}

#[derive(Debug)]
pub struct CompiledGlob {
    segments: Vec<SegmentMatcher>,
}

impl CompiledGlob {
    /// 文字列をパースしてCompiledGlobを生成します。
    pub fn new(pattern: &str) -> io::Result<Self> {
        let parsed = Path::new(pattern).parse_dot()?;
        let is_absolute = parsed.is_absolute();
        let pattern = parsed.to_str().unwrap().to_string();
        let pattern = Arc::new(pattern);
        let mut segments = Vec::new();

        let mut seg_start = 0usize;
        fn push_segment_range(
            segments: &mut Vec<SegmentMatcher>,
            pattern: &Arc<String>,
            range: Range<usize>,
        ) {
            if range.is_empty() {
                return;
            }
            let seg = &pattern[range.clone()];
            if let Some(rel_pos) = seg.find("**") {
                if seg == "**" {
                    segments.push(SegmentMatcher::Descend);
                    return;
                }
                let abs_pos = range.start + rel_pos;
                let pre = range.start..abs_pos;
                let post = (abs_pos + 2)..range.end;
                if pre.is_empty() && !post.is_empty() {
                    segments.push(SegmentMatcher::Descend);
                    let mut tail = String::from("*");
                    tail.push_str(&pattern[post]);
                    segments.push(SegmentMatcher::WildMatch(WildMatch::new(&tail)));
                    return;
                }
                if !pre.is_empty() && post.is_empty() {
                    let mut head = pattern[pre].to_string();
                    head.push('*');
                    segments.push(SegmentMatcher::WildMatch(WildMatch::new(&head)));
                    segments.push(SegmentMatcher::Descend);
                    return;
                }
                if !pre.is_empty() && !post.is_empty() {
                    let mut head = pattern[pre].to_string();
                    head.push('*');
                    segments.push(SegmentMatcher::WildMatch(WildMatch::new(&head)));
                    segments.push(SegmentMatcher::Descend);
                    let mut tail = String::from("*");
                    tail.push_str(&pattern[post]);
                    segments.push(SegmentMatcher::WildMatch(WildMatch::new(&tail)));
                    return;
                }
                return;
            }

            let has_wild = seg.chars().any(|ch| matches!(ch, '*' | '?'));
            if has_wild {
                segments.push(SegmentMatcher::WildMatch(WildMatch::new(seg)));
            } else if let Some(SegmentMatcher::AnyPath(last)) = segments.last_mut() {
                last.range.end = range.end;
            } else {
                segments.push(SegmentMatcher::AnyPath(PathInner {
                    pathbase: pattern.clone(),
                    range,
                }));
            }
        }

        for (idx, ch) in pattern.char_indices() {
            if ch == MAIN_SEPARATOR {
                push_segment_range(&mut segments, &pattern, seg_start..idx);
                seg_start = idx + ch.len_utf8();
            }
        }
        push_segment_range(&mut segments, &pattern, seg_start..pattern.len());

        if !is_absolute {
            let pathbase = Arc::new(CWD.to_str().unwrap().to_string());
            let range = 0..pathbase.len();
            segments.insert(
                // TODO: This is O(n). Would you have any better idea?
                0,
                SegmentMatcher::AnyPath(PathInner { pathbase, range }),
            );
        }
        Ok(CompiledGlob { segments })
    }

    /// 固定文字列がマッチするかどうかを判定します。
    pub fn r#match(&self, path: &OsStr) -> bool {
        fn any_path_parts(text: &str) -> impl Iterator<Item = &str> {
            text.split(MAIN_SEPARATOR).filter(|s| !s.is_empty())
        }

        fn matches_from(
            compiled: &CompiledGlob,
            seg_idx: usize,
            path_idx: usize,
            path_parts: &[&str],
            memo: &mut [Option<bool>],
        ) -> bool {
            let width = path_parts.len() + 1;
            let memo_idx = seg_idx * width + path_idx;
            if let Some(v) = memo[memo_idx] {
                return v;
            }

            let result = if seg_idx == compiled.segments.len() {
                path_idx == path_parts.len()
            } else {
                match &compiled.segments[seg_idx] {
                    SegmentMatcher::AnyPath(inner) => {
                        let literal = &inner.pathbase[inner.range.clone()];
                        let mut current = path_idx;
                        let mut ok = true;
                        for expected in any_path_parts(literal) {
                            if current >= path_parts.len() || path_parts[current] != expected {
                                ok = false;
                                break;
                            }
                            current += 1;
                        }
                        ok && matches_from(compiled, seg_idx + 1, current, path_parts, memo)
                    }
                    SegmentMatcher::WildMatch(matcher) => {
                        path_parts
                            .get(path_idx)
                            .is_some_and(|part| matcher.matches(part))
                            && matches_from(compiled, seg_idx + 1, path_idx + 1, path_parts, memo)
                    }
                    SegmentMatcher::Descend => {
                        let mut next_seg = seg_idx + 1;
                        while next_seg < compiled.segments.len()
                            && matches!(compiled.segments[next_seg], SegmentMatcher::Descend)
                        {
                            next_seg += 1;
                        }
                        if next_seg == compiled.segments.len() {
                            true
                        } else {
                            (path_idx..=path_parts.len())
                                .any(|i| matches_from(compiled, next_seg, i, path_parts, memo))
                        }
                    }
                }
            };

            memo[memo_idx] = Some(result);
            result
        }

        let normalized = match Path::new(path).parse_dot() {
            Ok(v) => v,
            Err(_) => return false,
        };
        let normalized = match normalized.to_str() {
            Some(v) => v,
            None => return false,
        };
        let path_parts: Vec<&str> = normalized
            .split(MAIN_SEPARATOR)
            .filter(|s| !s.is_empty())
            .collect();

        let mut memo = vec![None; (self.segments.len() + 1) * (path_parts.len() + 1)];
        matches_from(self, 0, 0, &path_parts, &mut memo)
    }

    pub(crate) fn segments(&self) -> &[SegmentMatcher] {
        &self.segments
    }
}

#[cfg(test)]
mod tests {
    use super::{CompiledGlob, SegmentMatcher};
    use path_dedot::CWD;
    use std::path::Path;

    #[test]
    fn expands_leading_double_star_in_segment() {
        let glob = CompiledGlob::new("/tmp/**.rs").expect("glob must parse");
        assert!(glob.r#match("/tmp/main.rs".as_ref()));
        assert!(glob.r#match("/tmp/src/lib.rs".as_ref()));
        assert!(!glob.r#match("/tmp/src/lib.ts".as_ref()));
    }

    #[test]
    fn expands_trailing_double_star_in_segment() {
        let glob = CompiledGlob::new("/tmp/tag-**").expect("glob must parse");
        assert!(glob.r#match("/tmp/tag-a".as_ref()));
        assert!(glob.r#match("/tmp/tag-a/b".as_ref()));
        assert!(!glob.r#match("/tmp/taga".as_ref()));
    }

    #[test]
    fn prepends_cwd_when_first_segment_is_not_anypath() {
        let glob = CompiledGlob::new("*.rs").expect("glob must parse");
        assert!(matches!(
            glob.segments().first(),
            Some(SegmentMatcher::AnyPath(_))
        ));
    }

    #[test]
    fn does_not_prepend_cwd_for_absolute_descend_glob() {
        let glob = CompiledGlob::new("/**").expect("glob must parse");
        assert!(!matches!(
            glob.segments().first(),
            Some(SegmentMatcher::AnyPath(_))
        ));
    }

    #[test]
    fn does_not_prepend_cwd_for_absolute_wildcard_glob() {
        let glob = CompiledGlob::new("/*/").expect("glob must parse");
        assert!(!matches!(
            glob.segments().first(),
            Some(SegmentMatcher::AnyPath(_))
        ));
    }

    #[test]
    fn prepends_cwd_for_relative_literal_prefix() {
        let glob = CompiledGlob::new("target/*").expect("glob must parse");
        let cwd = CWD.to_str().expect("cwd must be valid utf-8");
        assert!(matches!(
            glob.segments().first(),
            Some(SegmentMatcher::AnyPath(inner)) if inner.as_str() == cwd
        ));

        let ok = format!("{}/target/file.txt", CWD.display());
        assert!(glob.r#match(Path::new(&ok).as_os_str()));
        assert!(!glob.r#match(Path::new("/target/file.txt").as_os_str()));
    }
}

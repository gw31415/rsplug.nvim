use path_dedot::{CWD, ParseDot};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt::Debug;
use std::io;
use std::ops::Range;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
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

impl Clone for PathInner {
    fn clone(&self) -> Self {
        Self {
            pathbase: Arc::clone(&self.pathbase),
            range: self.range.clone(),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) enum SegmentMatcher {
    AnyPath(PathInner),
    WildMatch { pattern: String, matcher: WildMatch },
    Descend,
}

#[derive(Debug, Clone)]
struct CompiledRule {
    rule_index: usize,
    is_exclude: bool,
    is_absolute: bool,
    segments: Vec<SegmentMatcher>,
}

#[allow(dead_code)]
#[derive(Debug, Default, Clone)]
struct RuleTerminal {
    rule_index: usize,
    is_exclude: bool,
}

type NodeId = usize;

#[derive(Debug, Default, Clone)]
struct TrieNode {
    literal_edges: hashbrown::HashMap<String, NodeId>,
    wild_edges: Vec<(String, WildMatch, NodeId)>,
    descend_edge: Option<NodeId>,
    terminals: Vec<RuleTerminal>,
}

#[derive(Debug, Clone)]
struct GlobTrie {
    nodes: Vec<TrieNode>,
}

impl GlobTrie {
    fn new() -> Self {
        Self {
            nodes: vec![TrieNode::default()],
        }
    }

    fn add_node(&mut self) -> NodeId {
        self.nodes.push(TrieNode::default());
        self.nodes.len() - 1
    }

    fn insert_rule(&mut self, rule: &CompiledRule) {
        fn any_path_parts(text: &str) -> impl Iterator<Item = &str> {
            text.split(MAIN_SEPARATOR).filter(|s| !s.is_empty())
        }

        let mut node = 0usize;
        for segment in &rule.segments {
            match segment {
                SegmentMatcher::AnyPath(inner) => {
                    for part in any_path_parts(inner.as_str()) {
                        if part.is_empty() {
                            continue;
                        }
                        let next = if let Some(existing) = self.nodes[node].literal_edges.get(part)
                        {
                            *existing
                        } else {
                            let created = self.add_node();
                            self.nodes[node]
                                .literal_edges
                                .insert(part.to_string(), created);
                            created
                        };
                        node = next;
                    }
                }
                SegmentMatcher::WildMatch {
                    pattern,
                    matcher: _,
                } => {
                    let mut next = None;
                    for (existing, _, node_id) in &self.nodes[node].wild_edges {
                        if existing == pattern {
                            next = Some(*node_id);
                            break;
                        }
                    }
                    let next = if let Some(node_id) = next {
                        node_id
                    } else {
                        let created = self.add_node();
                        self.nodes[node].wild_edges.push((
                            pattern.clone(),
                            WildMatch::new(pattern),
                            created,
                        ));
                        created
                    };
                    node = next;
                }
                SegmentMatcher::Descend => {
                    let next = if let Some(existing) = self.nodes[node].descend_edge {
                        existing
                    } else {
                        let created = self.add_node();
                        self.nodes[node].descend_edge = Some(created);
                        created
                    };
                    node = next;
                }
            }
        }
        self.nodes[node].terminals.push(RuleTerminal {
            rule_index: rule.rule_index,
            is_exclude: rule.is_exclude,
        });
    }
}

#[derive(Debug, Clone)]
pub struct CompiledGlob {
    ordered_rules: Vec<CompiledRule>,
    trie: GlobTrie,
    epsilon_closures: Vec<Vec<usize>>,
}

impl CompiledGlob {
    /// 文字列をパースしてCompiledGlobを生成します。
    pub fn new(pattern: &str) -> io::Result<Self> {
        if pattern.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pattern must not be empty",
            ));
        }
        let is_exclude = pattern.starts_with('!');
        let pattern_body = if is_exclude { &pattern[1..] } else { pattern };
        if pattern_body.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "exclude pattern must include body after '!'",
            ));
        }

        let parsed = Path::new(pattern_body).parse_dot()?;
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
                    segments.push(SegmentMatcher::WildMatch {
                        pattern: tail.clone(),
                        matcher: WildMatch::new(&tail),
                    });
                    return;
                }
                if !pre.is_empty() && post.is_empty() {
                    let mut head = pattern[pre].to_string();
                    head.push('*');
                    segments.push(SegmentMatcher::WildMatch {
                        pattern: head.clone(),
                        matcher: WildMatch::new(&head),
                    });
                    segments.push(SegmentMatcher::Descend);
                    return;
                }
                if !pre.is_empty() && !post.is_empty() {
                    let mut head = pattern[pre].to_string();
                    head.push('*');
                    segments.push(SegmentMatcher::WildMatch {
                        pattern: head.clone(),
                        matcher: WildMatch::new(&head),
                    });
                    segments.push(SegmentMatcher::Descend);
                    let mut tail = String::from("*");
                    tail.push_str(&pattern[post]);
                    segments.push(SegmentMatcher::WildMatch {
                        pattern: tail.clone(),
                        matcher: WildMatch::new(&tail),
                    });
                    return;
                }
                return;
            }

            let has_wild = seg.chars().any(|ch| matches!(ch, '*' | '?'));
            if has_wild {
                segments.push(SegmentMatcher::WildMatch {
                    pattern: seg.to_string(),
                    matcher: WildMatch::new(seg),
                });
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
        let mut compiled = CompiledGlob {
            ordered_rules: Vec::new(),
            trie: GlobTrie::new(),
            epsilon_closures: Vec::new(),
        };
        compiled.push_rule(segments, is_exclude, is_absolute);
        Ok(compiled)
    }

    pub fn merge(mut self, other: CompiledGlob) -> CompiledGlob {
        let base = self.ordered_rules.len();
        for (offset, mut rule) in other.ordered_rules.into_iter().enumerate() {
            rule.rule_index = base + offset;
            self.trie.insert_rule(&rule);
            self.ordered_rules.push(rule);
        }
        self.rebuild_epsilon_closure_cache();
        self
    }

    pub fn merge_many(globs: impl IntoIterator<Item = CompiledGlob>) -> io::Result<CompiledGlob> {
        let mut iter = globs.into_iter();
        let Some(mut merged) = iter.next() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "merge_many requires at least one compiled glob",
            ));
        };
        for glob in iter {
            merged = merged.merge(glob);
        }
        Ok(merged)
    }

    pub(crate) fn initial_states(&self) -> Vec<usize> {
        self.expand_epsilon_nodes([0usize].as_ref())
    }

    pub(crate) fn states_for_path(&self, path: &Path) -> Vec<usize> {
        let mut states = self.initial_states();
        for part in path
            .to_string_lossy()
            .split(MAIN_SEPARATOR)
            .filter(|s| !s.is_empty())
        {
            states = self.advance_states(&states, part);
            if states.is_empty() {
                break;
            }
        }
        states
    }

    pub(crate) fn start_paths(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut seen = hashbrown::HashSet::new();

        for rule in &self.ordered_rules {
            if rule.is_exclude {
                continue;
            }

            let mut prefix = PathBuf::new();
            for seg in &rule.segments {
                match seg {
                    SegmentMatcher::AnyPath(part) => {
                        prefix.push(part.as_ref());
                    }
                    SegmentMatcher::WildMatch { .. } | SegmentMatcher::Descend => break,
                }
            }

            let mut candidate = if prefix.as_os_str().is_empty() {
                PathBuf::from(MAIN_SEPARATOR.to_string())
            } else {
                prefix
            };
            if rule.is_absolute && !candidate.is_absolute() {
                candidate = PathBuf::from(MAIN_SEPARATOR.to_string()).join(candidate);
            }

            if seen.insert(candidate.clone()) {
                out.push(candidate);
            }
        }

        if out.is_empty() {
            out.push(PathBuf::from(MAIN_SEPARATOR.to_string()));
        }

        out
    }

    pub(crate) fn advance_states(&self, current: &[usize], part: &str) -> Vec<usize> {
        let expanded = self.expand_epsilon_nodes(current);
        let mut next = Vec::new();
        let mut overflow_seen: Option<HashSet<usize>> = None;
        for node_idx in expanded {
            let node = &self.trie.nodes[node_idx];
            if let Some(next_idx) = node.literal_edges.get(part) {
                push_unique_state(&mut next, &mut overflow_seen, *next_idx);
            }
            for (_, matcher, next_idx) in &node.wild_edges {
                if matcher.matches(part) {
                    push_unique_state(&mut next, &mut overflow_seen, *next_idx);
                }
            }
            if node.descend_edge.is_some() {
                push_unique_state(&mut next, &mut overflow_seen, node_idx);
            }
        }
        self.expand_epsilon_nodes(&next)
    }

    pub(crate) fn is_match_state(&self, current: &[usize]) -> bool {
        matches!(self.match_decision(current), Some(true))
    }

    #[allow(dead_code)]
    pub(crate) fn literal_candidates(&self, current: &[usize]) -> Vec<String> {
        let expanded = self.expand_epsilon_nodes(current);
        let mut out = hashbrown::HashSet::new();
        for node_idx in expanded {
            out.extend(self.trie.nodes[node_idx].literal_edges.keys().cloned());
        }
        let mut out = out.into_iter().collect::<Vec<_>>();
        out.sort_unstable();
        out
    }

    pub(crate) fn needs_directory_scan(&self, current: &[usize]) -> bool {
        let expanded = self.expand_epsilon_nodes(current);
        expanded.into_iter().any(|node_idx| {
            let node = &self.trie.nodes[node_idx];
            !node.wild_edges.is_empty() || node.descend_edge.is_some()
        })
    }

    fn push_rule(&mut self, segments: Vec<SegmentMatcher>, is_exclude: bool, is_absolute: bool) {
        let rule = CompiledRule {
            rule_index: self.ordered_rules.len(),
            is_exclude,
            is_absolute,
            segments,
        };
        self.trie.insert_rule(&rule);
        self.ordered_rules.push(rule);
        self.rebuild_epsilon_closure_cache();
    }

    fn expand_epsilon_nodes(&self, current: &[usize]) -> Vec<usize> {
        if current.is_empty() {
            return Vec::new();
        }
        if current.len() == 1 {
            if let Some(cached) = self.epsilon_closures.get(current[0]) {
                return cached.clone();
            }
            return Vec::new();
        }

        let mut out = Vec::new();
        let mut overflow_seen: Option<HashSet<usize>> = None;
        for node_idx in current {
            let Some(cached) = self.epsilon_closures.get(*node_idx) else {
                continue;
            };
            for value in cached {
                push_unique_state(&mut out, &mut overflow_seen, *value);
            }
        }
        out.sort_unstable();
        out
    }

    fn rebuild_epsilon_closure_cache(&mut self) {
        let node_count = self.trie.nodes.len();
        self.epsilon_closures = vec![Vec::new(); node_count];
        for node_idx in 0..node_count {
            let mut closure = Vec::new();
            let mut cursor = Some(node_idx);
            while let Some(idx) = cursor {
                closure.push(idx);
                cursor = self.trie.nodes[idx].descend_edge;
            }
            closure.sort_unstable();
            closure.dedup();
            self.epsilon_closures[node_idx] = closure;
        }
    }

    fn match_decision(&self, current: &[usize]) -> Option<bool> {
        let expanded = self.expand_epsilon_nodes(current);
        let mut selected: Option<(usize, bool)> = None;
        for node_idx in expanded {
            for terminal in &self.trie.nodes[node_idx].terminals {
                if selected
                    .as_ref()
                    .is_none_or(|(idx, _)| terminal.rule_index >= *idx)
                {
                    selected = Some((terminal.rule_index, !terminal.is_exclude));
                }
            }
        }
        selected.map(|(_, include)| include)
    }

    /// 固定文字列がマッチするかどうかを判定します。
    pub fn r#match(&self, path: &OsStr) -> bool {
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
        let mut states = self.initial_states();
        for part in path_parts {
            states = self.advance_states(&states, part);
            if states.is_empty() {
                return false;
            }
        }
        self.match_decision(&states).unwrap_or(false)
    }

    #[allow(dead_code)]
    pub(crate) fn segments(&self) -> &[SegmentMatcher] {
        assert!(
            self.ordered_rules.len() <= 1,
            "segments() is only available for single-rule CompiledGlob"
        );
        self.ordered_rules
            .first()
            .map(|rule| rule.segments.as_slice())
            .unwrap_or(&[])
    }

    #[allow(dead_code)]
    pub(crate) fn ordered_rules_segments(&self) -> Vec<&[SegmentMatcher]> {
        self.ordered_rules
            .iter()
            .map(|rule| rule.segments.as_slice())
            .collect()
    }
}

const INLINE_STATE_DEDUP_LIMIT: usize = 16;

fn push_unique_state(
    out: &mut Vec<usize>,
    overflow_seen: &mut Option<HashSet<usize>>,
    value: usize,
) {
    if let Some(seen) = overflow_seen.as_mut() {
        if seen.insert(value) {
            out.push(value);
        }
        return;
    }

    if out.contains(&value) {
        return;
    }
    out.push(value);

    if out.len() > INLINE_STATE_DEDUP_LIMIT {
        let mut seen = HashSet::with_capacity(out.len() * 2);
        for existing in out.iter().copied() {
            seen.insert(existing);
        }
        *overflow_seen = Some(seen);
    }
}

#[cfg(test)]
mod tests {
    use super::{CompiledGlob, SegmentMatcher};
    use path_dedot::CWD;
    use std::io;
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

    #[test]
    fn merge_many_or_union_matches() {
        let one = CompiledGlob::new("/tmp/**/*.rs").expect("glob must parse");
        let two = CompiledGlob::new("/tmp/**/*.txt").expect("glob must parse");
        let merged = CompiledGlob::merge_many(vec![one, two]).expect("must merge");
        assert!(merged.r#match("/tmp/a/b/main.rs".as_ref()));
        assert!(merged.r#match("/tmp/a/b/readme.txt".as_ref()));
        assert!(!merged.r#match("/tmp/a/b/readme.md".as_ref()));
    }

    #[test]
    fn merge_preserves_rule_order() {
        let one = CompiledGlob::new("/tmp/**/*.rs").expect("glob must parse");
        let two = CompiledGlob::new("/tmp/**/*.txt").expect("glob must parse");
        let merged = one.merge(two);
        assert_eq!(merged.ordered_rules.len(), 2);
        assert_eq!(merged.ordered_rules[0].rule_index, 0);
        assert_eq!(merged.ordered_rules[1].rule_index, 1);
    }

    #[test]
    fn descend_dedup_equivalence_under_merge() {
        let one = CompiledGlob::new("a/**/**/b").expect("glob must parse");
        let two = CompiledGlob::new("a/**/b").expect("glob must parse");
        let merged = one.merge(two);
        let canonical = CompiledGlob::new("a/**/b").expect("glob must parse");
        for path in ["a/b", "a/x/b", "a/x/y/b", "a/x/y/c"] {
            assert_eq!(
                merged.r#match(path.as_ref()),
                canonical.r#match(path.as_ref())
            );
        }
    }

    #[test]
    fn empty_merge_many_is_invalid_input() {
        let merged = CompiledGlob::merge_many(Vec::new());
        assert!(matches!(
            merged,
            Err(err) if err.kind() == io::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn supports_exclude_with_last_match_wins() {
        let include = CompiledGlob::new("/tmp/**/*.txt").expect("glob must parse");
        let exclude = CompiledGlob::new("!/tmp/**/ignore.txt").expect("glob must parse");
        let reinclude = CompiledGlob::new("/tmp/**/ignore.txt").expect("glob must parse");

        let merged =
            CompiledGlob::merge_many(vec![include, exclude, reinclude]).expect("must merge");
        assert!(merged.r#match("/tmp/a/keep.txt".as_ref()));
        assert!(merged.r#match("/tmp/a/ignore.txt".as_ref()));
    }

    #[test]
    fn exclude_can_remove_previous_include() {
        let include = CompiledGlob::new("/tmp/**/*.txt").expect("glob must parse");
        let exclude = CompiledGlob::new("!/tmp/**/ignore.txt").expect("glob must parse");
        let merged = CompiledGlob::merge_many(vec![include, exclude]).expect("must merge");
        assert!(merged.r#match("/tmp/a/keep.txt".as_ref()));
        assert!(!merged.r#match("/tmp/a/ignore.txt".as_ref()));
    }

    #[test]
    fn reject_empty_pattern_and_bare_exclude() {
        assert!(matches!(
            CompiledGlob::new(""),
            Err(err) if err.kind() == io::ErrorKind::InvalidInput
        ));
        assert!(matches!(
            CompiledGlob::new("!"),
            Err(err) if err.kind() == io::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn start_paths_include_static_prefix_of_include_rules() {
        let glob = CompiledGlob::new("/tmp/root/**.rs").expect("glob must parse");
        let starts = glob.start_paths();
        assert!(starts.iter().any(|p| p == Path::new("/tmp/root")));
    }

    #[test]
    fn states_for_path_keeps_descend_capability() {
        let glob = CompiledGlob::new("/tmp/root/**.rs").expect("glob must parse");
        let states = glob.states_for_path(Path::new("/tmp/root"));
        assert!(!states.is_empty());
        let leaf_states = glob.advance_states(&states, "main.rs");
        assert!(glob.is_match_state(&leaf_states));
    }
}

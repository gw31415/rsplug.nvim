use std::io;
use std::path::Path;

use wildmatch::WildMatch;

#[derive(Debug, Clone)]
pub struct PatternRule {
    pub is_exclude: bool,
    segments: Vec<SegmentMatcher>,
}

#[derive(Debug, Clone)]
enum SegmentMatcher {
    AnyPath(String),
    Glob(WildMatch),
    Descends,
}

#[derive(Debug, Clone)]
pub struct CompiledRules {
    pub ordered_rules: Vec<PatternRule>,
    pub include_prefixes: Vec<String>,
    pub include_patterns: Vec<String>,
}

#[derive(Debug)]
pub struct InitializedPattern {
    index: usize,
    rule: PatternRule,
    include_prefix: Option<String>,
    include_pattern: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NormalizedPattern {
    pub is_exclude: bool,
    pub pattern: String,
}

pub fn initialize_pattern(
    index: usize,
    raw_pattern: String,
    cwd_prefixes: &[String],
) -> io::Result<InitializedPattern> {
    let Some(normalized) = normalize_raw_pattern(raw_pattern.as_str(), cwd_prefixes)? else {
        return Ok(InitializedPattern {
            index,
            rule: PatternRule {
                is_exclude: raw_pattern.starts_with('!'),
                segments: Vec::new(),
            },
            include_prefix: None,
            include_pattern: None,
        });
    };

    let include_prefix = if normalized.is_exclude {
        None
    } else {
        Some(extract_static_prefix(&normalized.pattern))
    };
    let include_pattern = if normalized.is_exclude {
        None
    } else {
        Some(normalized.pattern.clone())
    };

    Ok(InitializedPattern {
        index,
        rule: PatternRule {
            is_exclude: normalized.is_exclude,
            segments: compile_matcher(&normalized.pattern)?,
        },
        include_prefix,
        include_pattern,
    })
}

pub fn normalize_raw_pattern(
    raw_pattern: &str,
    cwd_prefixes: &[String],
) -> io::Result<Option<NormalizedPattern>> {
    if raw_pattern.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Pattern must not be empty",
        ));
    }

    let is_exclude = raw_pattern.starts_with('!');
    let pattern_body = if is_exclude {
        &raw_pattern[1..]
    } else {
        raw_pattern
    };
    if pattern_body.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Exclude pattern must include body after '!'",
        ));
    }

    let Some(resolved_pattern) = resolve_pattern_for_cwd(pattern_body, cwd_prefixes) else {
        return Ok(None);
    };
    let normalized_pattern = normalize_pattern(&resolved_pattern);
    Ok(Some(NormalizedPattern {
        is_exclude,
        pattern: normalized_pattern,
    }))
}

pub fn compile_rules(mut initialized_patterns: Vec<InitializedPattern>) -> CompiledRules {
    initialized_patterns.sort_by_key(|entry| entry.index);

    let mut ordered_rules = Vec::with_capacity(initialized_patterns.len());
    let mut include_prefixes = Vec::new();
    let mut include_patterns = Vec::new();

    for entry in initialized_patterns {
        ordered_rules.push(entry.rule);
        if let Some(prefix) = entry.include_prefix {
            include_prefixes.push(prefix);
        }
        if let Some(pattern) = entry.include_pattern {
            include_patterns.push(pattern);
        }
    }

    CompiledRules {
        ordered_rules,
        include_prefixes,
        include_patterns,
    }
}

pub fn matches_last_rule(path: &str, rules: &[PatternRule]) -> bool {
    let mut selected = false;
    for rule in rules {
        if rule.is_match(path) {
            selected = !rule.is_exclude;
        }
    }
    selected
}

pub fn could_match_subtree(directory_relative_path: &str, include_prefixes: &[String]) -> bool {
    if include_prefixes.is_empty() {
        return false;
    }
    if directory_relative_path.is_empty() {
        return true;
    }

    for prefix in include_prefixes {
        if prefix.is_empty() {
            return true;
        }
        if is_path_prefix(directory_relative_path, prefix)
            || is_path_prefix(prefix, directory_relative_path)
        {
            return true;
        }
    }

    false
}

fn normalize_pattern(input: &str) -> String {
    let mut normalized = input.replace('\\', "/");
    normalized = normalized.trim_start_matches("./").to_string();
    normalized = normalized.trim_start_matches('/').to_string();
    normalized = normalized.replace("/**.", "/**/*.");
    if normalized.starts_with("**.") {
        normalized = format!("**/*{}", &normalized[2..]);
    }
    normalized
}

fn compile_matcher(pattern: &str) -> io::Result<Vec<SegmentMatcher>> {
    let mut segments = Vec::new();
    if pattern.is_empty() {
        return Ok(segments);
    }

    for segment in pattern.split('/') {
        if segment.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Pattern contains empty path segment: {pattern}"),
            ));
        }
        if segment == "**" {
            if !matches!(segments.last(), Some(SegmentMatcher::Descends)) {
                segments.push(SegmentMatcher::Descends);
            }
            continue;
        }

        if segment_contains_wildcard(segment) {
            segments.push(SegmentMatcher::Glob(WildMatch::new(segment)));
        } else {
            segments.push(SegmentMatcher::AnyPath(segment.to_string()));
        }
    }

    Ok(segments)
}

fn resolve_pattern_for_cwd(pattern: &str, cwd_prefixes: &[String]) -> Option<String> {
    if !Path::new(pattern).is_absolute() {
        let cwd = cwd_prefixes.first()?;
        let cwd = cwd.trim_end_matches('/');
        return Some(format!("{cwd}/{pattern}"));
    }
    Some(pattern.to_string())
}

fn extract_static_prefix(pattern: &str) -> String {
    if pattern.is_empty() {
        return String::new();
    }

    let mut segments = Vec::new();
    for segment in pattern.split('/') {
        if segment.is_empty() || segment_contains_wildcard(segment) {
            break;
        }
        segments.push(segment);
    }
    segments.join("/")
}

fn segment_contains_wildcard(segment: &str) -> bool {
    segment.contains('*') || segment.contains('?') || segment.contains('[')
}

fn is_path_prefix(prefix: &str, path: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

impl PatternRule {
    fn is_match(&self, path: &str) -> bool {
        if self.segments.is_empty() {
            return false;
        }
        let path_segments = if path.is_empty() {
            Vec::new()
        } else {
            path.split('/').collect::<Vec<_>>()
        };
        let mut failed_states = hashbrown::HashSet::new();
        segments_match_from(&self.segments, 0, 0, &path_segments, &mut failed_states)
    }
}

fn segments_match_from(
    segments: &[SegmentMatcher],
    pattern_index: usize,
    path_index: usize,
    path_segments: &[&str],
    failed_states: &mut hashbrown::HashSet<(usize, usize)>,
) -> bool {
    if !failed_states.insert((pattern_index, path_index)) {
        return false;
    }

    let Some(segment) = segments.get(pattern_index) else {
        return path_index == path_segments.len();
    };

    match segment {
        SegmentMatcher::AnyPath(expected) => {
            if path_segments
                .get(path_index)
                .is_some_and(|actual| actual == expected)
            {
                return segments_match_from(
                    segments,
                    pattern_index + 1,
                    path_index + 1,
                    path_segments,
                    failed_states,
                );
            }
            false
        }
        SegmentMatcher::Glob(matcher) => {
            if path_segments
                .get(path_index)
                .is_some_and(|actual| matcher.matches(actual))
            {
                return segments_match_from(
                    segments,
                    pattern_index + 1,
                    path_index + 1,
                    path_segments,
                    failed_states,
                );
            }
            false
        }
        SegmentMatcher::Descends => {
            if segments_match_from(
                segments,
                pattern_index + 1,
                path_index,
                path_segments,
                failed_states,
            ) {
                return true;
            }
            if path_index < path_segments.len() {
                return segments_match_from(
                    segments,
                    pattern_index,
                    path_index + 1,
                    path_segments,
                    failed_states,
                );
            }
            false
        }
    }
}

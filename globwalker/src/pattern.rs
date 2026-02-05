use std::io;
use std::path::{Component, Path};

use globset::{GlobBuilder, GlobMatcher};

#[derive(Debug, Clone)]
pub struct PatternRule {
    pub is_exclude: bool,
    matcher: RuleMatcher,
}

#[derive(Debug, Clone)]
enum RuleMatcher {
    Never,
    AnyPath,
    PrefixAndSuffix { prefix: String, suffix: String },
    Glob(GlobMatcher),
}

#[derive(Debug, Clone)]
pub struct CompiledRules {
    pub ordered_rules: Vec<PatternRule>,
    pub include_prefixes: Vec<String>,
}

#[derive(Debug)]
pub struct InitializedPattern {
    index: usize,
    rule: PatternRule,
    include_prefix: Option<String>,
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
                matcher: RuleMatcher::Never,
            },
            include_prefix: None,
        });
    };

    let include_prefix = if normalized.is_exclude {
        None
    } else {
        Some(extract_static_prefix(&normalized.pattern))
    };

    Ok(InitializedPattern {
        index,
        rule: PatternRule {
            is_exclude: normalized.is_exclude,
            matcher: compile_matcher(&normalized.pattern)?,
        },
        include_prefix,
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

    for entry in initialized_patterns {
        ordered_rules.push(entry.rule);
        if let Some(prefix) = entry.include_prefix {
            include_prefixes.push(prefix);
        }
    }

    CompiledRules {
        ordered_rules,
        include_prefixes,
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
    normalized = normalized.replace("/**.", "/**/*.");
    if normalized.starts_with("**.") {
        normalized = format!("**/*{}", &normalized[2..]);
    }
    normalized
}

fn compile_matcher(pattern: &str) -> io::Result<RuleMatcher> {
    if pattern == "**" {
        return Ok(RuleMatcher::AnyPath);
    }
    if let Some(matcher) = try_build_prefix_suffix_matcher(pattern) {
        return Ok(matcher);
    }

    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| RuleMatcher::Glob(glob.compile_matcher()))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))
}

fn try_build_prefix_suffix_matcher(pattern: &str) -> Option<RuleMatcher> {
    if let Some(extension) = pattern.strip_prefix("**/*.")
        && is_literal_fragment(extension)
    {
        return Some(RuleMatcher::PrefixAndSuffix {
            prefix: String::new(),
            suffix: format!(".{extension}"),
        });
    }

    let (prefix, extension) = pattern.split_once("/**/*.")?;
    if !is_literal_fragment(prefix) || !is_literal_fragment(extension) {
        return None;
    }
    Some(RuleMatcher::PrefixAndSuffix {
        prefix: prefix.to_string(),
        suffix: format!(".{extension}"),
    })
}

fn is_literal_fragment(value: &str) -> bool {
    !value.is_empty() && !segment_contains_wildcard(value)
}

fn resolve_pattern_for_cwd(pattern: &str, cwd_prefixes: &[String]) -> Option<String> {
    if !Path::new(pattern).is_absolute() {
        return Some(pattern.to_string());
    }

    let normalized_pattern = pattern.replace('\\', "/");
    for normalized_cwd in cwd_prefixes {
        if normalized_pattern == *normalized_cwd {
            return Some(String::new());
        }
        let with_separator = format!("{normalized_cwd}/");
        if let Some(relative) = normalized_pattern.strip_prefix(&with_separator) {
            return Some(relative.to_string());
        }
        if let Some(relative) =
            resolve_absolute_pattern_relative_to_cwd(normalized_cwd, &normalized_pattern)
        {
            return Some(relative);
        }
    }
    None
}

fn resolve_absolute_pattern_relative_to_cwd(cwd: &str, absolute_pattern: &str) -> Option<String> {
    let cwd_path = Path::new(cwd);
    let pattern_path = Path::new(absolute_pattern);
    if !cwd_path.is_absolute() || !pattern_path.is_absolute() {
        return None;
    }

    let cwd_components = cwd_path
        .components()
        .filter(|component| !matches!(component, Component::CurDir))
        .collect::<Vec<_>>();
    let pattern_components = pattern_path
        .components()
        .filter(|component| !matches!(component, Component::CurDir))
        .collect::<Vec<_>>();

    let mut common_length = 0usize;
    while common_length < cwd_components.len()
        && common_length < pattern_components.len()
        && cwd_components[common_length] == pattern_components[common_length]
    {
        common_length += 1;
    }

    let mut relative_parts = Vec::new();
    for component in &cwd_components[common_length..] {
        if matches!(component, Component::Normal(_) | Component::ParentDir) {
            relative_parts.push("..".to_string());
        }
    }
    for component in &pattern_components[common_length..] {
        match component {
            Component::Normal(value) => relative_parts.push(value.to_string_lossy().into_owned()),
            Component::ParentDir => relative_parts.push("..".to_string()),
            _ => {}
        }
    }

    Some(relative_parts.join("/"))
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
        match &self.matcher {
            RuleMatcher::Never => false,
            RuleMatcher::AnyPath => true,
            RuleMatcher::PrefixAndSuffix { prefix, suffix } => {
                path.ends_with(suffix)
                    && (prefix.is_empty()
                        || path == prefix
                        || path
                            .strip_prefix(prefix)
                            .is_some_and(|rest| rest.starts_with('/')))
            }
            RuleMatcher::Glob(matcher) => matcher.is_match(path),
        }
    }
}

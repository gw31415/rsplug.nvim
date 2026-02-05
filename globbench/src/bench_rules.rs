use std::io;
use std::path::Path;

use globwalker::pattern::{
    CompiledRules, PatternRule, compile_rules, initialize_pattern, matches_last_rule,
};

pub(crate) fn compile_benchmark_rules(
    raw_patterns: &[String],
    cwd: &Path,
) -> io::Result<CompiledRules> {
    let cwd_prefixes = build_cwd_prefixes(cwd);
    let mut initialized = Vec::with_capacity(raw_patterns.len());

    for (index, raw_pattern) in raw_patterns.iter().cloned().enumerate() {
        initialized.push(initialize_pattern(index, raw_pattern, &cwd_prefixes)?);
    }

    Ok(compile_rules(initialized))
}

pub(crate) fn matches_compiled_rules(path: &str, rules: &[PatternRule]) -> bool {
    matches_last_rule(path, rules)
}

fn build_cwd_prefixes(cwd: &Path) -> Vec<String> {
    vec![cwd.to_string_lossy().replace('\\', "/")]
}

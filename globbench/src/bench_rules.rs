use std::io;
use std::path::Path;

use walker::compiled_glob::CompiledGlob;

pub(crate) fn compile_benchmark_rules(
    raw_patterns: &[String],
    _cwd: &Path,
) -> io::Result<CompiledGlob> {
    let mut compiled = Vec::with_capacity(raw_patterns.len());
    for raw_pattern in raw_patterns {
        compiled.push(CompiledGlob::new(raw_pattern)?);
    }
    CompiledGlob::merge_many(compiled)
}

pub(crate) fn matches_compiled_rules(path: &Path, rules: &CompiledGlob) -> bool {
    rules.r#match(path.as_os_str())
}

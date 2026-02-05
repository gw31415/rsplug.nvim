use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::bench_rules::compile_benchmark_rules;
use crate::bench_runners::run_benchmark_attempt;
use crate::bench_types::{AttemptOutcome, BenchmarkAccumulator, BenchmarkKind, BenchmarkResult};

pub(crate) const BENCHMARK_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const BENCHMARK_RUNS: usize = 3;
const BENCHMARK_KINDS: [BenchmarkKind; 3] = [
    BenchmarkKind::Globwalker,
    BenchmarkKind::IgnoreParallel,
    BenchmarkKind::Glob,
];

pub(crate) async fn run_and_print(cwd: &Path, raw_patterns: &[String]) -> io::Result<()> {
    let rules = Arc::new(compile_benchmark_rules(raw_patterns, cwd)?);
    let results = run_benchmarks(cwd, raw_patterns, rules).await;

    for result in &results {
        if let Some(error) = &result.error {
            println!("{}: error ({error})", result.name);
            continue;
        }
        if result.timed_out {
            println!(
                "{}: timed out after {}s",
                result.name,
                BENCHMARK_TIMEOUT.as_secs()
            );
            continue;
        }
        println!(
            "{}: avg {:?} over {} runs ({} files)",
            result.name,
            result
                .average_elapsed
                .ok_or_else(|| io::Error::other("missing average elapsed"))?,
            BENCHMARK_RUNS,
            result
                .matched_files
                .ok_or_else(|| io::Error::other("missing matched files"))?
        );
    }

    if let Some(fastest) = results
        .iter()
        .filter(|result| !result.timed_out && result.error.is_none())
        .min_by_key(|result| result.average_elapsed)
    {
        println!(
            "fastest: {} ({:?})",
            fastest.name,
            fastest
                .average_elapsed
                .ok_or_else(|| io::Error::other("missing fastest elapsed"))?
        );
    }

    report_count_mismatch(&results);
    Ok(())
}

async fn run_benchmarks(
    cwd: &Path,
    raw_patterns: &[String],
    rules: Arc<globwalker::pattern::CompiledRules>,
) -> Vec<BenchmarkResult> {
    let mut accumulators = vec![BenchmarkAccumulator::default(); BENCHMARK_KINDS.len()];

    for round in 0..BENCHMARK_RUNS {
        for kind in benchmark_round_order(round) {
            let accumulator = &mut accumulators[kind as usize];
            if accumulator.timed_out || accumulator.error.is_some() {
                continue;
            }

            let attempt =
                run_benchmark_attempt(kind, cwd, raw_patterns, &rules, BENCHMARK_TIMEOUT).await;
            match attempt {
                Ok(AttemptOutcome::TimedOut) => {
                    accumulator.timed_out = true;
                    accumulator.matched_files = None;
                }
                Ok(AttemptOutcome::Completed(attempt)) => {
                    accumulator.elapsed_total += attempt.elapsed;
                    accumulator.completed_runs += 1;
                    match accumulator.matched_files {
                        None => accumulator.matched_files = Some(attempt.matched_files),
                        Some(previous) if previous == attempt.matched_files => {}
                        Some(previous) => {
                            accumulator.error = Some(format!(
                                "matched files changed between runs: {previous} -> {}",
                                attempt.matched_files
                            ));
                            accumulator.matched_files = None;
                        }
                    }
                }
                Err(error) => {
                    accumulator.error = Some(error.to_string());
                    accumulator.matched_files = None;
                }
            }
        }
    }

    BENCHMARK_KINDS
        .iter()
        .copied()
        .map(|kind| {
            let accumulator = &accumulators[kind as usize];
            if let Some(error) = accumulator.error.clone() {
                return BenchmarkResult {
                    name: kind.name(),
                    average_elapsed: None,
                    matched_files: None,
                    timed_out: false,
                    error: Some(error),
                };
            }
            if accumulator.timed_out {
                return BenchmarkResult {
                    name: kind.name(),
                    average_elapsed: None,
                    matched_files: None,
                    timed_out: true,
                    error: None,
                };
            }
            if accumulator.completed_runs != BENCHMARK_RUNS {
                return BenchmarkResult {
                    name: kind.name(),
                    average_elapsed: None,
                    matched_files: None,
                    timed_out: false,
                    error: Some(format!(
                        "incomplete benchmark runs: expected {BENCHMARK_RUNS}, got {}",
                        accumulator.completed_runs
                    )),
                };
            }
            BenchmarkResult {
                name: kind.name(),
                average_elapsed: Some(
                    accumulator.elapsed_total / accumulator.completed_runs as u32,
                ),
                matched_files: accumulator.matched_files,
                timed_out: false,
                error: None,
            }
        })
        .collect()
}

fn benchmark_round_order(round: usize) -> Vec<BenchmarkKind> {
    let mut order = BENCHMARK_KINDS.to_vec();
    let order_len = order.len();
    order.rotate_left(round % order_len);
    order
}

fn report_count_mismatch(results: &[BenchmarkResult]) {
    let completed: Vec<_> = results
        .iter()
        .filter(|result| !result.timed_out && result.error.is_none())
        .collect();

    if completed.is_empty() {
        return;
    }

    let baseline = completed[0].matched_files;
    if completed
        .iter()
        .all(|result| result.matched_files == baseline)
    {
        if let Some(files) = baseline {
            println!("matched files: all implementations agree ({files})");
        }
        return;
    }

    println!("matched files mismatch:");
    for result in completed {
        if let Some(files) = result.matched_files {
            println!("  - {}: {}", result.name, files);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench_runners::run_benchmark;
    use crate::bench_types::AttemptResult;
    use tokio::time::{Duration, sleep};

    #[tokio::test]
    async fn timeout_result_does_not_prevent_other_benchmark_results() {
        let slow = run_benchmark(Duration::from_millis(5), || async {
            sleep(Duration::from_millis(50)).await;
            Ok(AttemptOutcome::Completed(AttemptResult {
                elapsed: Duration::from_millis(50),
                matched_files: 1,
            }))
        })
        .await
        .expect("slow benchmark attempt must produce an outcome");
        let fast = run_benchmark(Duration::from_millis(20), || async {
            Ok(AttemptOutcome::Completed(AttemptResult {
                elapsed: Duration::from_millis(1),
                matched_files: 1,
            }))
        })
        .await
        .expect("fast benchmark attempt must produce an outcome");

        assert!(matches!(slow, AttemptOutcome::TimedOut));
        assert!(matches!(fast, AttemptOutcome::Completed(_)));
    }

    #[test]
    fn benchmark_order_rotates_each_round() {
        assert_eq!(
            benchmark_round_order(0)
                .into_iter()
                .map(BenchmarkKind::name)
                .collect::<Vec<_>>(),
            vec!["globwalker", "ignore(parallel)", "glob"]
        );
        assert_eq!(
            benchmark_round_order(1)
                .into_iter()
                .map(BenchmarkKind::name)
                .collect::<Vec<_>>(),
            vec!["ignore(parallel)", "glob", "globwalker"]
        );
        assert_eq!(
            benchmark_round_order(2)
                .into_iter()
                .map(BenchmarkKind::name)
                .collect::<Vec<_>>(),
            vec!["glob", "globwalker", "ignore(parallel)"]
        );
    }
}

use std::time::Duration;

#[derive(Debug)]
pub(crate) struct BenchmarkResult {
    pub(crate) name: &'static str,
    pub(crate) average_elapsed: Option<Duration>,
    pub(crate) matched_files: Option<usize>,
    pub(crate) timed_out: bool,
    pub(crate) error: Option<String>,
}

#[derive(Debug)]
pub(crate) struct AttemptResult {
    pub(crate) elapsed: Duration,
    pub(crate) matched_files: usize,
}

#[derive(Debug)]
pub(crate) enum AttemptOutcome {
    Completed(AttemptResult),
    TimedOut,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct BenchmarkAccumulator {
    pub(crate) elapsed_total: Duration,
    pub(crate) matched_files: Option<usize>,
    pub(crate) timed_out: bool,
    pub(crate) error: Option<String>,
    pub(crate) completed_runs: usize,
}

#[derive(Copy, Clone, Debug)]
#[repr(usize)]
pub(crate) enum BenchmarkKind {
    IgnoreParallel = 0,
    Walker = 1,
}

impl BenchmarkKind {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::IgnoreParallel => "ignore(parallel)",
            Self::Walker => "walker",
        }
    }
}

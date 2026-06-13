use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::sync::Notify;

const DEFAULT_INITIAL_LIMIT: usize = 16;
const DEFAULT_MIN_LIMIT: usize = 1;
const DEFAULT_MAX_LIMIT: usize = 256;
const INITIAL_ADJUST_INTERVAL: Duration = Duration::from_millis(64);
const MIN_ADJUST_INTERVAL: Duration = Duration::from_millis(64);
const MAX_ADJUST_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct AdaptiveSemaphore {
    state: Arc<State>,
}

#[derive(Debug)]
struct State {
    inner: Mutex<Inner>,
    notify: Notify,
}

#[derive(Debug)]
struct Inner {
    limit: usize,
    min_limit: usize,
    max_limit: usize,
    adjust_interval: Duration,
    last_adjusted_at: Instant,
    in_flight: usize,
    current: WindowStats,
    previous: Option<Snapshot>,
}

#[derive(Debug)]
pub struct AdaptiveSemaphorePermit {
    semaphore: AdaptiveSemaphore,
    started_at: Instant,
    finished: bool,
}

#[derive(Debug, Default)]
struct WindowStats {
    completed: usize,
    errors: usize,
    total_latency: Duration,
}

#[derive(Clone, Copy, Debug)]
struct Snapshot {
    throughput: f64,
    avg_latency: Duration,
}

impl Default for AdaptiveSemaphore {
    fn default() -> Self {
        Self::new()
    }
}

impl AdaptiveSemaphore {
    pub fn new() -> Self {
        Self::with_limits(
            DEFAULT_INITIAL_LIMIT,
            DEFAULT_MIN_LIMIT,
            DEFAULT_MAX_LIMIT,
            INITIAL_ADJUST_INTERVAL,
        )
    }

    pub fn with_limits(
        initial_limit: usize,
        min_limit: usize,
        max_limit: usize,
        adjust_interval: Duration,
    ) -> Self {
        let min_limit = min_limit.max(1);
        let max_limit = max_limit.max(min_limit);
        let limit = initial_limit.clamp(min_limit, max_limit);
        let adjust_interval = adjust_interval.clamp(MIN_ADJUST_INTERVAL, MAX_ADJUST_INTERVAL);
        Self {
            state: Arc::new(State {
                inner: Mutex::new(Inner {
                    limit,
                    min_limit,
                    max_limit,
                    adjust_interval,
                    last_adjusted_at: Instant::now(),
                    in_flight: 0,
                    current: WindowStats::default(),
                    previous: None,
                }),
                notify: Notify::new(),
            }),
        }
    }

    #[cfg(test)]
    fn limit(&self) -> usize {
        self.state.inner.lock().unwrap().limit
    }

    #[cfg(test)]
    fn adjust_interval(&self) -> Duration {
        self.state.inner.lock().unwrap().adjust_interval
    }

    pub async fn acquire(&self) -> AdaptiveSemaphorePermit {
        loop {
            let notified = {
                let mut inner = self.state.inner.lock().unwrap();
                if inner.in_flight < inner.limit {
                    inner.in_flight += 1;
                    return AdaptiveSemaphorePermit {
                        semaphore: self.clone(),
                        started_at: Instant::now(),
                        finished: false,
                    };
                }
                self.state.notify.notified()
            };
            notified.await;
        }
    }

    fn release(&self, outcome: Option<(Duration, bool)>) {
        let should_notify = {
            let mut inner = self.state.inner.lock().unwrap();
            inner.in_flight = inner.in_flight.saturating_sub(1);
            if let Some((latency, is_error)) = outcome {
                inner.record(latency, is_error);
                inner.adjust_if_needed();
            }
            inner.in_flight < inner.limit
        };
        if should_notify {
            self.state.notify.notify_one();
        }
    }

    #[cfg(test)]
    fn record(&self, latency: Duration, is_error: bool) {
        self.state.inner.lock().unwrap().record(latency, is_error);
    }

    #[cfg(test)]
    fn force_adjust_for(&self, elapsed: Duration) -> bool {
        let changed = self.state.inner.lock().unwrap().adjust(elapsed);
        if changed {
            self.state.notify.notify_waiters();
        }
        changed
    }
}

impl AdaptiveSemaphorePermit {
    pub fn finish(mut self, is_error: bool) {
        let elapsed = self.started_at.elapsed();
        self.finished = true;
        self.semaphore.release(Some((elapsed, is_error)));
    }
}

impl Drop for AdaptiveSemaphorePermit {
    fn drop(&mut self) {
        if !self.finished {
            self.semaphore.release(None);
        }
    }
}

impl Inner {
    fn record(&mut self, latency: Duration, is_error: bool) {
        self.current.completed += 1;
        self.current.total_latency += latency;
        self.current.errors += usize::from(is_error);
    }

    fn adjust_if_needed(&mut self) -> bool {
        let elapsed = self.last_adjusted_at.elapsed();
        if elapsed < self.adjust_interval {
            return false;
        }
        self.adjust(elapsed)
    }

    fn adjust(&mut self, elapsed: Duration) -> bool {
        if self.current.completed == 0 {
            self.last_adjusted_at = Instant::now();
            return false;
        }

        let snapshot = self.current.snapshot(elapsed);
        let changed = if let Some(previous) = self.previous {
            let error_rate = self.current.error_rate();
            self.update_adjust_interval(previous, snapshot, error_rate);
            self.update_limit(previous, snapshot, error_rate)
        } else {
            false
        };

        self.previous = Some(snapshot);
        self.current = WindowStats::default();
        self.last_adjusted_at = Instant::now();
        changed
    }

    fn update_limit(&mut self, previous: Snapshot, current: Snapshot, error_rate: f64) -> bool {
        let old_limit = self.limit;
        if error_rate > 0.01
            || current.throughput < previous.throughput * 0.95
            || current.avg_latency > previous.avg_latency.mul_f64(1.5)
        {
            self.limit = (self.limit / 2).max(self.min_limit);
        } else if current.throughput > previous.throughput * 1.05
            && current.avg_latency < previous.avg_latency.mul_f64(1.25)
        {
            self.limit = (self.limit + 1).min(self.max_limit);
        }
        self.limit != old_limit
    }

    fn update_adjust_interval(&mut self, previous: Snapshot, current: Snapshot, error_rate: f64) {
        let volatility = current.volatility_since(previous).max(error_rate);
        self.adjust_interval = if volatility >= 0.5 {
            (self.adjust_interval / 2).max(MIN_ADJUST_INTERVAL)
        } else if volatility >= 0.15 {
            self.adjust_interval
        } else {
            (self.adjust_interval * 2).min(MAX_ADJUST_INTERVAL)
        };
    }
}

impl WindowStats {
    fn snapshot(&self, elapsed: Duration) -> Snapshot {
        Snapshot {
            throughput: self.completed as f64 / elapsed.as_secs_f64().max(f64::EPSILON),
            avg_latency: self.total_latency / self.completed as u32,
        }
    }

    fn error_rate(&self) -> f64 {
        self.errors as f64 / self.completed as f64
    }
}

impl Snapshot {
    fn volatility_since(self, previous: Self) -> f64 {
        self.throughput
            .relative_change_from(previous.throughput)
            .max(self.avg_latency.relative_change_from(previous.avg_latency))
    }
}

trait RelativeChange {
    fn relative_change_from(self, previous: Self) -> f64;
}

impl RelativeChange for f64 {
    fn relative_change_from(self, previous: Self) -> f64 {
        ((self - previous) / previous.abs().max(f64::EPSILON)).abs()
    }
}

impl RelativeChange for Duration {
    fn relative_change_from(self, previous: Self) -> f64 {
        self.as_secs_f64()
            .relative_change_from(previous.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn semaphore(initial_limit: usize) -> AdaptiveSemaphore {
        AdaptiveSemaphore::with_limits(initial_limit, 1, 256, MIN_ADJUST_INTERVAL)
    }

    fn record_window(
        semaphore: &AdaptiveSemaphore,
        completed: usize,
        latency: Duration,
        errors: usize,
        elapsed: Duration,
    ) {
        for i in 0..completed {
            semaphore.record(latency, i < errors);
        }
        semaphore.force_adjust_for(elapsed);
    }

    #[test]
    fn increases_when_throughput_improves_without_latency_regression() {
        let semaphore = semaphore(16);
        record_window(
            &semaphore,
            100,
            Duration::from_millis(10),
            0,
            Duration::from_secs(1),
        );
        record_window(
            &semaphore,
            110,
            Duration::from_millis(11),
            0,
            Duration::from_secs(1),
        );
        assert_eq!(semaphore.limit(), 17);
    }

    #[test]
    fn halves_when_throughput_regresses() {
        let semaphore = semaphore(16);
        record_window(
            &semaphore,
            100,
            Duration::from_millis(10),
            0,
            Duration::from_secs(1),
        );
        record_window(
            &semaphore,
            90,
            Duration::from_millis(10),
            0,
            Duration::from_secs(1),
        );
        assert_eq!(semaphore.limit(), 8);
    }

    #[test]
    fn halves_when_latency_regresses_sharply() {
        let semaphore = semaphore(16);
        record_window(
            &semaphore,
            100,
            Duration::from_millis(10),
            0,
            Duration::from_secs(1),
        );
        record_window(
            &semaphore,
            100,
            Duration::from_millis(16),
            0,
            Duration::from_secs(1),
        );
        assert_eq!(semaphore.limit(), 8);
    }

    #[test]
    fn respects_min_and_max_limits() {
        let min_semaphore = AdaptiveSemaphore::with_limits(1, 1, 256, Duration::ZERO);
        record_window(
            &min_semaphore,
            100,
            Duration::from_millis(10),
            0,
            Duration::from_secs(1),
        );
        record_window(
            &min_semaphore,
            90,
            Duration::from_millis(10),
            0,
            Duration::from_secs(1),
        );
        assert_eq!(min_semaphore.limit(), 1);

        let max_semaphore = AdaptiveSemaphore::with_limits(256, 1, 256, Duration::ZERO);
        record_window(
            &max_semaphore,
            100,
            Duration::from_millis(10),
            0,
            Duration::from_secs(1),
        );
        record_window(
            &max_semaphore,
            110,
            Duration::from_millis(11),
            0,
            Duration::from_secs(1),
        );
        assert_eq!(max_semaphore.limit(), 256);
    }

    #[test]
    fn halves_when_error_rate_is_high() {
        let semaphore = semaphore(16);
        record_window(
            &semaphore,
            100,
            Duration::from_millis(10),
            0,
            Duration::from_secs(1),
        );
        record_window(
            &semaphore,
            100,
            Duration::from_millis(10),
            2,
            Duration::from_secs(1),
        );
        assert_eq!(semaphore.limit(), 8);
    }

    #[test]
    fn shortens_adjust_interval_when_metrics_swing() {
        let semaphore = AdaptiveSemaphore::with_limits(16, 1, 256, Duration::from_millis(200));
        record_window(
            &semaphore,
            100,
            Duration::from_millis(10),
            0,
            Duration::from_secs(1),
        );
        record_window(
            &semaphore,
            50,
            Duration::from_millis(20),
            0,
            Duration::from_secs(1),
        );
        assert_eq!(semaphore.adjust_interval(), Duration::from_millis(100));
    }

    #[test]
    fn lengthens_adjust_interval_when_metrics_are_stable() {
        let semaphore = AdaptiveSemaphore::with_limits(16, 1, 256, Duration::from_millis(100));
        record_window(
            &semaphore,
            100,
            Duration::from_millis(10),
            0,
            Duration::from_secs(1),
        );
        record_window(
            &semaphore,
            102,
            Duration::from_millis(10),
            0,
            Duration::from_secs(1),
        );
        assert_eq!(semaphore.adjust_interval(), Duration::from_millis(200));
    }

    #[test]
    fn clamps_adjust_interval() {
        let min_semaphore = AdaptiveSemaphore::with_limits(16, 1, 256, Duration::from_millis(1));
        assert_eq!(min_semaphore.adjust_interval(), MIN_ADJUST_INTERVAL);

        let max_semaphore = AdaptiveSemaphore::with_limits(16, 1, 256, Duration::from_secs(10));
        assert_eq!(max_semaphore.adjust_interval(), MAX_ADJUST_INTERVAL);
    }
}

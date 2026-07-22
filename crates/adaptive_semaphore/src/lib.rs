use std::{
    collections::HashMap,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

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
    blocking_notify: Condvar,
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
    /// Permits released by cancellation/drop without `finish`. Tracked separately
    /// so a cancelled in-flight operation never reads as a successful sample
    /// (PLANS U2 step 1) and never skews throughput/latency adaptation.
    cancelled: usize,
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
                blocking_notify: Condvar::new(),
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

    /// Current-window completed samples (success + error), excluding cancellations.
    #[cfg(test)]
    fn completed(&self) -> usize {
        self.state.inner.lock().unwrap().current.completed
    }

    /// Current-window error samples.
    #[cfg(test)]
    fn errors(&self) -> usize {
        self.state.inner.lock().unwrap().current.errors
    }

    /// Current-window cancelled (dropped-without-finish) samples.
    #[cfg(test)]
    fn cancelled(&self) -> usize {
        self.state.inner.lock().unwrap().current.cancelled
    }

    /// Permits currently held by in-flight operations.
    #[cfg(test)]
    fn in_flight(&self) -> usize {
        self.state.inner.lock().unwrap().in_flight
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

    pub fn blocking_acquire(&self) -> AdaptiveSemaphorePermit {
        let mut inner = self.state.inner.lock().unwrap();
        loop {
            if inner.in_flight < inner.limit {
                inner.in_flight += 1;
                return AdaptiveSemaphorePermit {
                    semaphore: self.clone(),
                    started_at: Instant::now(),
                    finished: false,
                };
            }
            inner = self.state.blocking_notify.wait(inner).unwrap();
        }
    }

    fn release(&self, outcome: Option<(Duration, bool)>) {
        let should_notify = {
            let mut inner = self.state.inner.lock().unwrap();
            inner.in_flight = inner.in_flight.saturating_sub(1);
            match outcome {
                // Completed operation: contribute exactly one success/error sample
                // and let the limit adapt.
                Some((latency, is_error)) => {
                    inner.record(latency, is_error);
                    inner.adjust_if_needed();
                }
                // Dropped without `finish`: the in-flight operation was cancelled
                // (task aborted, future dropped, etc.). Count it as cancelled
                // rather than leaving no trace and rather than as a success.
                None => inner.current.cancelled += 1,
            }
            inner.in_flight < inner.limit
        };
        if should_notify {
            self.state.notify.notify_one();
            self.state.blocking_notify.notify_one();
        }
    }

    /// Run `future` under one adaptive permit and record exactly one outcome
    /// sample (success/error). This is the single path network call sites must
    /// use so the advertised limit actually adapts on the main network path
    /// (PLANS U2 step 1).
    ///
    /// If the returned future is dropped after a permit was acquired but before
    /// completion, the permit is dropped without `finish` and is counted as
    /// cancelled instead of as a successful sample.
    pub async fn run<F, T, E>(&self, future: F) -> Result<T, E>
    where
        F: std::future::Future<Output = Result<T, E>>,
    {
        let permit = self.acquire().await;
        let result = future.await;
        permit.finish(result.is_err());
        result
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
            self.state.blocking_notify.notify_all();
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

/// Per-host concurrency cap layered on top of the global [`AdaptiveSemaphore`]
/// (PLANS U2 step 6).
///
/// Each distinct host string gets its own bounded [`Semaphore`] of `cap`
/// permits, so one host (e.g. `api.github.com`) cannot exhaust the global
/// budget and starve requests to another host (e.g. `codeload` or an arbitrary
/// Git host). The global [`AdaptiveSemaphore`] remains the upper bound.
#[derive(Clone, Debug)]
pub struct HostLimits {
    cap: usize,
    slots: Arc<Mutex<HashMap<Arc<str>, Arc<Semaphore>>>>,
}

impl HostLimits {
    /// Create a per-host limiter allowing at most `cap` concurrent operations
    /// per host. `cap` is clamped to at least 1.
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            slots: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Acquire one permit for `host`, creating the host's semaphore on first use.
    pub async fn acquire(&self, host: Arc<str>) -> HostPermit {
        let slot = {
            let mut slots = self.slots.lock().unwrap();
            slots
                .entry(host)
                .or_insert_with(|| Arc::new(Semaphore::new(self.cap)))
                .clone()
        };
        let permit = slot
            .acquire_owned()
            .await
            .expect("host semaphore is never closed");
        HostPermit { _permit: permit }
    }

    /// Number of distinct hosts that currently own a semaphore.
    #[cfg(test)]
    fn host_count(&self) -> usize {
        self.slots.lock().unwrap().len()
    }
}

/// A held per-host slot. Releases on drop.
pub struct HostPermit {
    _permit: OwnedSemaphorePermit,
}

/// Combined network concurrency budget: a global adaptive upper bound plus a
/// per-host cap (PLANS U2 steps 1 and 6).
///
/// Every network operation must go through [`NetworkLimits::run`] (or
/// [`NetworkLimits::acquire`] + [`NetworkPermit::finish`]) so that it consumes
/// one global permit, one host permit, and contributes exactly one adaptive
/// success/error sample.
#[derive(Clone, Debug)]
pub struct NetworkLimits {
    global: AdaptiveSemaphore,
    hosts: HostLimits,
}

impl NetworkLimits {
    pub fn new(global: AdaptiveSemaphore, per_host_cap: usize) -> Self {
        Self {
            global,
            hosts: HostLimits::new(per_host_cap),
        }
    }

    pub fn global(&self) -> &AdaptiveSemaphore {
        &self.global
    }

    pub fn hosts(&self) -> &HostLimits {
        &self.hosts
    }

    /// Acquire a global adaptive permit plus a per-host permit. Call
    /// [`NetworkPermit::finish`] to record the outcome sample; dropping the
    /// guard without finishing counts as cancelled.
    pub async fn acquire(&self, host: &str) -> NetworkPermit {
        let host = Arc::<str>::from(host);
        let host_permit = self.hosts.acquire(host).await;
        let adaptive = self.global.acquire().await;
        NetworkPermit {
            adaptive,
            _host: host_permit,
        }
    }

    /// Run `future` under a global + per-host permit and record exactly one
    /// outcome sample. Cancellation of the returned future counts as cancelled.
    ///
    /// The host slot is acquired first and held across the global permit + the
    /// operation, so the per-host cap bounds concurrent in-flight requests to a
    /// single host regardless of how the global limit adapts.
    pub async fn run<F, T, E>(&self, host: &str, future: F) -> Result<T, E>
    where
        F: std::future::Future<Output = Result<T, E>>,
    {
        let host = Arc::<str>::from(host);
        let _host = self.hosts.acquire(host).await;
        self.global.run(future).await
    }
}

/// Combined global adaptive permit + per-host slot.
pub struct NetworkPermit {
    adaptive: AdaptiveSemaphorePermit,
    _host: HostPermit,
}

impl NetworkPermit {
    /// Record the outcome sample (success/error) and release both permits.
    pub fn finish(self, is_error: bool) {
        let NetworkPermit { adaptive, _host } = self;
        // Release the host slot first, then record the adaptive sample.
        drop(_host);
        adaptive.finish(is_error);
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

    #[test]
    fn blocking_acquire_waits_for_release() {
        let semaphore = AdaptiveSemaphore::with_limits(1, 1, 1, MIN_ADJUST_INTERVAL);
        let permit = semaphore.blocking_acquire();
        let waiting = semaphore.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let permit = waiting.blocking_acquire();
            tx.send(()).expect("test receiver should be alive");
            permit.finish(false);
        });

        assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
        permit.finish(false);
        rx.recv_timeout(Duration::from_secs(1))
            .expect("blocking waiter should be notified");
        handle.join().expect("waiter should not panic");
    }

    // --- U2: adaptive permit outcome recording (real call-site path) ---

    #[tokio::test]
    async fn run_records_one_success_sample_per_completion() {
        // `run` is the path network call sites use. A successful completion must
        // contribute exactly one (success) sample and release the permit.
        let sem = semaphore(8);
        for _ in 0..5 {
            sem.run(async { Ok::<_, ()>(()) }).await.unwrap();
        }
        assert_eq!(sem.completed(), 5);
        assert_eq!(sem.errors(), 0);
        assert_eq!(sem.cancelled(), 0);
        assert_eq!(sem.in_flight(), 0);
    }

    #[tokio::test]
    async fn run_records_one_error_sample_per_completion() {
        let sem = semaphore(8);
        for _ in 0..5 {
            let _ = sem.run(async { Err::<(), _>(()) }).await;
        }
        assert_eq!(sem.completed(), 5);
        assert_eq!(sem.errors(), 5);
        assert_eq!(sem.cancelled(), 0);
    }

    #[tokio::test]
    async fn run_halves_limit_after_synthetic_error_window() {
        // Proves the production acquire -> await -> finish path (not direct
        // record()/force_adjust_for unit calls) drives adaptation: a sustained
        // error window halves the limit.
        let sem = semaphore(8);
        // Baseline success window.
        for _ in 0..10 {
            sem.run(async { Ok::<_, ()>(()) }).await.unwrap();
        }
        sem.force_adjust_for(Duration::from_secs(1));
        // Error window: error_rate = 1.0 > 0.01 -> halve.
        for _ in 0..10 {
            let _ = sem.run(async { Err::<(), _>(()) }).await;
        }
        sem.force_adjust_for(Duration::from_secs(1));
        assert_eq!(sem.limit(), 4, "sustained errors must halve the limit");
    }

    #[tokio::test]
    async fn dropped_permit_without_finish_counts_as_cancelled() {
        // A permit dropped without finish is a cancellation, not a silent release
        // and not a successful sample.
        let sem = semaphore(8);
        {
            let _permit = sem.acquire().await;
        }
        assert_eq!(sem.cancelled(), 1);
        assert_eq!(sem.completed(), 0);
        assert_eq!(sem.in_flight(), 0);
    }

    #[tokio::test]
    async fn dropped_run_future_counts_as_cancelled() {
        // Aborting a task that is inside `run` (permit acquired, op pending)
        // drops the permit mid-flight and counts it as cancelled.
        let sem = AdaptiveSemaphore::with_limits(1, 1, 1, MIN_ADJUST_INTERVAL);
        let sem2 = sem.clone();
        let handle =
            tokio::spawn(async move { sem2.run(std::future::pending::<Result<(), ()>>()).await });
        // Wait for the spawned task to acquire its permit.
        let mut acquired = false;
        for _ in 0..200 {
            if sem.in_flight() == 1 {
                acquired = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(acquired, "task should have acquired the permit");
        handle.abort();
        let _ = handle.await;
        assert_eq!(sem.cancelled(), 1);
        assert_eq!(sem.in_flight(), 0);
    }

    // --- U2: per-host + global resource limits ---

    #[tokio::test]
    async fn host_limits_caps_concurrent_per_host() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hosts = HostLimits::new(2);
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..10 {
            let hosts = hosts.clone();
            let in_flight = in_flight.clone();
            let max_seen = max_seen.clone();
            handles.push(tokio::spawn(async move {
                let _p = hosts.acquire(Arc::<str>::from("api.github.com")).await;
                let cur = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(cur, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let max = max_seen.load(Ordering::SeqCst);
        assert!(max <= 2, "per-host cap exceeded: {max}");
        assert!(max >= 2, "expected to reach the cap, got {max}");
        assert_eq!(hosts.host_count(), 1);
    }

    #[tokio::test]
    async fn host_limits_isolate_hosts() {
        // A saturated host must not block a different host.
        let hosts = HostLimits::new(1);
        let _a = hosts.acquire(Arc::<str>::from("a.example")).await;
        let b = tokio::time::timeout(
            Duration::from_millis(100),
            hosts.acquire(Arc::<str>::from("b.example")),
        )
        .await;
        assert!(b.is_ok(), "different host must not be blocked");
        assert_eq!(hosts.host_count(), 2);
    }

    #[tokio::test]
    async fn network_limits_run_records_sample_and_uses_host_slot() {
        let net = NetworkLimits::new(
            AdaptiveSemaphore::with_limits(8, 1, 64, MIN_ADJUST_INTERVAL),
            2,
        );
        for _ in 0..3 {
            net.run("api.github.com", async { Ok::<_, ()>(()) })
                .await
                .unwrap();
        }
        assert_eq!(net.global().completed(), 3);
        assert_eq!(net.hosts().host_count(), 1);
    }
}

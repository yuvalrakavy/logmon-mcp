//! Rate limits for log lines something outside the broker can repeat at will — a malformed
//! message, a failed accept — so the log cannot be filled from outside, and the stderr writer
//! that cannot panic.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// How often a throttled line may be logged.
const INTERVAL_NANOS: i64 = 60_000_000_000;

/// Counts occurrences of one thing and says which to log: the first, then at most one a minute,
/// each carrying the running count.
///
/// Time-based on purpose. Logging the 1st, 2nd, 4th, 8th… occurrence of a never-reset count
/// bounds the log just as well, but goes silent for longer and longer: a broker that had seen a
/// thousand malformed messages in some old burst said nothing about the next thousand — a newly
/// misconfigured sender at one a second went unmentioned for seventeen minutes.
pub(crate) struct Throttle {
    count: AtomicU64,
    last: AtomicI64,
}

impl Throttle {
    pub(crate) const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            last: AtomicI64::new(i64::MIN),
        }
    }

    /// Count one occurrence; `Some(total so far)` when this one should be logged.
    ///
    /// On the monotonic clock: on the wall clock, a step backwards (an NTP correction, a
    /// sleeping laptop's resync) would leave the last-logged time in the future, and every
    /// line silenced until the clock caught up with it.
    pub(crate) fn hit(&self) -> Option<u64> {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let elapsed = START.get_or_init(std::time::Instant::now).elapsed();
        self.hit_at(i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX))
    }

    fn hit_at(&self, now_nanos: i64) -> Option<u64> {
        let n = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        let last = self.last.load(Ordering::Relaxed);
        (now_nanos.saturating_sub(last) >= INTERVAL_NANOS
            && self
                .last
                .compare_exchange(last, now_nanos, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok())
        .then_some(n)
    }
}

/// Write a line to stderr without ever panicking. `eprintln!` panics when stderr cannot be
/// written — a full disk under the auto-started broker's log file — and the panic killed the
/// task that logged: a connection, or an accept loop.
pub(crate) fn note(args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{args}");
}

/// What a loop around an accept or a receive does after one fails: log it (throttled), and
/// pause if the failure repeats. A one-off failure — a peer that reset before it was accepted —
/// is retried at once. One that repeats — out of file descriptors, typically — returned the
/// same error at once on every retry: a spin at full CPU, and a log line per turn, for as long
/// as it lasted. `failures_in_a_row` is the loop's own, reset by it on a success.
pub(crate) async fn pace_after_error(
    throttle: &Throttle,
    failures_in_a_row: &mut u32,
    log: impl FnOnce(u64),
) {
    *failures_in_a_row = failures_in_a_row.saturating_add(1);
    if let Some(n) = throttle.hit() {
        log(n);
    }
    if *failures_in_a_row > 1 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: i64 = 1_000_000_000;

    /// The first occurrence is logged, then at most one a minute — and a line that is logged
    /// carries every occurrence since the start, so the ones in between are not lost.
    #[test]
    fn logs_the_first_then_at_most_one_a_minute_with_the_running_count() {
        let t = Throttle::new();
        let start = 1_000 * SEC;
        assert_eq!(t.hit_at(start), Some(1));
        assert_eq!(t.hit_at(start + SEC), None);
        assert_eq!(t.hit_at(start + 59 * SEC), None);
        assert_eq!(t.hit_at(start + 60 * SEC), Some(4));
        assert_eq!(t.hit_at(start + 61 * SEC), None);
        // Never silent for longer than a minute, however much came before.
        for i in 0..1_000 {
            let _ = t.hit_at(start + 62 * SEC + i);
        }
        assert_eq!(t.hit_at(start + 121 * SEC), Some(1_006));
    }

    /// A repeated failure pauses before the next attempt; a single one does not.
    #[tokio::test(start_paused = true)]
    async fn only_a_repeated_failure_pauses() {
        let t = Throttle::new();
        let mut failures = 0;
        let start = tokio::time::Instant::now();
        pace_after_error(&t, &mut failures, |_| {}).await;
        assert_eq!(start.elapsed(), std::time::Duration::ZERO);
        pace_after_error(&t, &mut failures, |_| {}).await;
        assert_eq!(start.elapsed(), std::time::Duration::from_millis(100));
    }
}

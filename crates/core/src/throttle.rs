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
        tokio::time::sleep(PAUSE).await;
    }
}

const PAUSE: std::time::Duration = std::time::Duration::from_millis(100);

/// [`pace_after_error`] for an accept loop that is not ours — a server that takes a stream of
/// accepted connections and retries a failed accept itself (tonic: with no pause, so out of
/// file descriptors it spun a core). Wrapping the stream paces it from outside: after a
/// repeated error, the next accept is not attempted until the pause has passed.
pub(crate) struct PacedAccepts<S> {
    inner: S,
    failures_in_a_row: u32,
    pause: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    throttle: &'static Throttle,
    what: &'static str,
}

impl<S> PacedAccepts<S> {
    pub(crate) fn new(inner: S, throttle: &'static Throttle, what: &'static str) -> Self {
        Self {
            inner,
            failures_in_a_row: 0,
            pause: None,
            throttle,
            what,
        }
    }
}

impl<S, T> tokio_stream::Stream for PacedAccepts<S>
where
    S: tokio_stream::Stream<Item = std::io::Result<T>> + Unpin,
{
    type Item = S::Item;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::future::Future;
        use std::task::Poll;
        if let Some(pause) = self.pause.as_mut() {
            if pause.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            self.pause = None;
        }
        let item = match std::pin::Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(item) => item,
            Poll::Pending => return Poll::Pending,
        };
        match &item {
            Some(Ok(_)) => self.failures_in_a_row = 0,
            Some(Err(e)) => {
                self.failures_in_a_row = self.failures_in_a_row.saturating_add(1);
                if let Some(n) = self.throttle.hit() {
                    note(format_args!(
                        "{} accept failed ({n} so far): {e}",
                        self.what
                    ));
                }
                if self.failures_in_a_row > 1 {
                    self.pause = Some(Box::pin(tokio::time::sleep(PAUSE)));
                }
            }
            None => {}
        }
        Poll::Ready(item)
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

    /// A paced stream of accepts hands every item through, and after a repeated error does not
    /// ask for the next one until the pause has passed; a success resets the count.
    #[tokio::test(start_paused = true)]
    async fn a_paced_accept_stream_pauses_only_after_a_repeated_error() {
        use tokio_stream::StreamExt;
        static T: Throttle = Throttle::new();
        let err = || Err::<u8, _>(std::io::Error::other("emfile"));
        let mut s = PacedAccepts::new(
            tokio_stream::iter(vec![err(), Ok(1), err(), err(), Ok(2)]),
            &T,
            "test",
        );
        let start = tokio::time::Instant::now();
        let mut seen = Vec::new();
        while let Some(item) = s.next().await {
            seen.push((item.is_ok(), start.elapsed()));
        }
        let ms = std::time::Duration::from_millis;
        assert_eq!(
            seen,
            vec![
                (false, ms(0)),
                (true, ms(0)),
                (false, ms(0)),  // one error after a success: no pause
                (false, ms(0)),  // the repeat arms the pause...
                (true, ms(100)), // ...which the next accept waits out
            ]
        );
    }
}

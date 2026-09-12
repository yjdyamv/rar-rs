//! Write-side progress reporting: the raw `WriteProgress` event trait and
//! the aggregated `(committed, total)` tracker behind the public callback.

/// A high-level archive-writing operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WriteOperation {
    /// Building a RAR 5 recovery record.
    Recovery,
}

/// Progress reported by archive writers.
///
/// Callbacks can be invoked concurrently when parallel compression is enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WriteProgressEvent {
    /// An operation has started.
    OperationStarted {
        operation: WriteOperation,
        total_bytes: Option<u64>,
        total_entries: Option<usize>,
        pass: usize,
    },
    /// Absolute progress within the current operation or pass.
    // Only emitted by the wire-level write path, which is `raw`-gated.
    #[cfg_attr(not(feature = "raw"), allow(dead_code))]
    Advanced {
        operation: WriteOperation,
        completed_bytes: u64,
        total_bytes: u64,
        pass: usize,
    },
    /// An operation has finished.
    OperationFinished {
        operation: WriteOperation,
        total_bytes: Option<u64>,
        total_entries: Option<usize>,
        pass: usize,
    },
}

/// Receives archive-writing progress events.
pub trait WriteProgress: Send + Sync {
    fn report(&self, event: WriteProgressEvent);

    /// Returns true when the caller wants the active write operation to stop.
    fn is_cancelled(&self) -> bool {
        false
    }
}

impl<F> WriteProgress for F
where
    F: Fn(WriteProgressEvent) + Send + Sync,
{
    fn report(&self, event: WriteProgressEvent) {
        self(event);
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ProgressReporter<'a>(pub(crate) &'a dyn WriteProgress);

impl std::fmt::Debug for ProgressReporter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProgressReporter(..)")
    }
}

impl ProgressReporter<'_> {
    pub(crate) fn report(self, event: WriteProgressEvent) {
        self.0.report(event);
    }

    #[allow(dead_code)]
    pub(crate) fn is_cancelled(self) -> bool {
        self.0.is_cancelled()
    }
}

/// Aggregates per-member progress into a single monotonic, operation-global
/// stream for the user callback.
///
/// The historical per-member contract reported `(done, file_total)` restarting
/// at `0` for every member, which forced consumers to stitch deltas together
/// themselves and made the delta bookkeeping break the moment multiple members
/// compressed concurrently (the `parallel` wave path). This tracker keeps a
/// per-member baseline internally so the user callback always observes
/// `(committed, total)` where `committed` only ever increases and `total` is
/// the operation's whole input byte count. It is `Sync` through the enclosing
/// mutex, so the same instance can be shared by the Rayon workers that
/// compress a wave and by the single-threaded write-back loop.
pub(crate) struct ProgressTracker {
    callback: Option<Box<dyn FnMut(u64, u64) + Send>>,
    total: u64,
    committed: u64,
    per_member: std::collections::HashMap<usize, u64>,
}

impl ProgressTracker {
    pub(crate) fn new(callback: Option<Box<dyn FnMut(u64, u64) + Send>>) -> Self {
        Self {
            callback,
            total: 0,
            committed: 0,
            per_member: std::collections::HashMap::new(),
        }
    }

    /// Set the operation-wide total (sum of every member's input size). When
    /// left at 0 the tracker adopts the first reported member size.
    pub(crate) fn set_total(&mut self, total: u64) {
        self.total = total;
    }

    /// Report `done` bytes of member `member` (member sizes are counted
    /// against `member_total`). Deltas are clamped so the emitted stream stays
    /// monotonic even when a member's progress temporarily goes backwards
    /// (e.g. the compress-pass spill of a member that later falls back to
    /// STORE and re-copies from the start).
    pub(crate) fn report(&mut self, member: usize, done: u64, member_total: u64) {
        if self.total == 0 {
            self.total = member_total;
        }
        let prev = self.per_member.get(&member).copied().unwrap_or(0);
        let delta = done.saturating_sub(prev);
        if delta == 0 {
            return;
        }
        self.per_member.insert(member, done);
        self.committed = self.committed.saturating_add(delta);
        if let Some(cb) = self.callback.as_mut() {
            let committed = self.committed.min(self.total);
            let total = self.total;
            // The callback is user code running while the enclosing mutex is
            // held: a panic unwinding through the guard would poison the lock
            // and turn every later `lock().expect(...)` in the write pipeline
            // into a second panic. Catch it so a misbehaving callback cannot
            // take down an otherwise healthy write operation.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                cb(committed, total);
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ProgressTracker;

    #[test]
    fn panicking_callback_does_not_escape_or_stop_progress() {
        let mut tracker = ProgressTracker::new(Some(Box::new(|_, _| panic!("user callback"))));
        tracker.report(0, 10, 10);
        tracker.report(0, 20, 20);
        assert_eq!(tracker.committed, 20);
    }
}

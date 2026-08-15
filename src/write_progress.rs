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

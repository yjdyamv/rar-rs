//! Parallel execution pools for the `parallel` feature.
//!
//! Mirrors the reference layout's `parallel` module: thread-pool plumbing
//! lives here so the format modules stay focused on the format. A small
//! dedicated compression pool keeps many-small-member batches fast; the
//! extraction pool sizes itself with [`set_extraction_threads`].
/// Compression thread count set with [`set_compression_threads`]
/// (like `rar -mt`); 0 = automatic sizing.
static COMPRESSION_THREADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Extraction thread count set with [`set_extraction_threads`];
/// 0 = automatic sizing.
static EXTRACTION_THREADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Set the compression thread count used by the `parallel` feature
/// (like `rar -mt<N>`). `0` restores automatic sizing.
pub fn set_compression_threads(threads: usize) {
    COMPRESSION_THREADS.store(threads, std::sync::atomic::Ordering::Relaxed);
}

/// Set the extraction thread count used by the `parallel` feature.
/// `0` restores automatic sizing.
pub fn set_extraction_threads(threads: usize) {
    EXTRACTION_THREADS.store(threads, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(feature = "parallel")]
pub(crate) fn configured_threads() -> Option<usize> {
    let n = COMPRESSION_THREADS.load(std::sync::atomic::Ordering::Relaxed);
    (n > 0).then_some(n)
}

#[cfg(feature = "parallel")]
pub(crate) fn configured_extraction_threads() -> Option<usize> {
    let n = EXTRACTION_THREADS.load(std::sync::atomic::Ordering::Relaxed);
    (n > 0).then_some(n)
}

/// Dedicated Rayon pools for batch compression, one per thread count.
///
/// Pools are cached by worker count so concurrent archives with different
/// `-mt` settings each run on their own pool (a single resizable pool would
/// make them stomp each other: one archive's `set_compression_threads`
/// would resize the pool out from under another in flight). The global
/// default setting selects the pool for archives without an override.
///
/// Compression uses *all* host cores by default (like `rar -mt0`): the
/// dedicated pool is sized from the host's available parallelism so
/// medium/large members get the full parallel win. Low-core machines
/// naturally get fewer workers. The host may cap the count explicitly with
/// `SA_RAR5_WASM_WORKERS` (wasm) / `set_compression_threads` (native) if a
/// smaller pool is desired; an archive may also pass a per-archive `threads`
/// override.
#[cfg(feature = "parallel")]
pub(crate) fn pool_threads(default: usize) -> usize {
    #[cfg(not(target_family = "wasm"))]
    {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(default)
    }
    // WASM cannot query the host CPU count (WASI reports 1 core), so follow
    // the emnapi worker-pool sizing and let the host override explicitly.
    // Precedence: SA_RAR5_WASM_WORKERS > NAPI_RS_ASYNC_WORK_POOL_SIZE >
    // UV_THREADPOOL_SIZE > `default`. The wasm loader sets SA_RAR5_WASM_WORKERS
    // from Node's os.availableParallelism() so the encoder uses every core.
    #[cfg(target_family = "wasm")]
    {
        let from_env = |key: &str| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
        };
        from_env("SA_RAR5_WASM_WORKERS")
            .or_else(|| from_env("NAPI_RS_ASYNC_WORK_POOL_SIZE"))
            .or_else(|| from_env("UV_THREADPOOL_SIZE"))
            .unwrap_or(default)
            .max(1)
    }
}

/// Default compression worker count: the `-mt` override when set,
/// otherwise automatic sizing to *all* host cores (maximum parallelism).
#[cfg(feature = "parallel")]
pub(crate) fn default_compression_threads() -> usize {
    configured_threads().unwrap_or_else(|| pool_threads(4))
}

/// Pool with exactly `threads` workers, cached per thread count (see the
/// module docs on why the pools are keyed by size).
#[cfg(feature = "parallel")]
pub(crate) fn compression_pool_for(threads: usize) -> std::sync::Arc<rayon::ThreadPool> {
    use std::sync::{Mutex, OnceLock};
    static POOLS: OnceLock<
        Mutex<std::collections::HashMap<usize, std::sync::Arc<rayon::ThreadPool>>>,
    > = OnceLock::new();
    let pools = POOLS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut map = pools.lock().expect("compression pools lock");
    map.entry(threads)
        .or_insert_with(|| std::sync::Arc::new(build_compression_pool(threads)))
        .clone()
}

/// The pool for the process-global default thread count (what
/// [`set_compression_threads`] selects for archives without an override).
#[cfg(feature = "parallel")]
pub(crate) fn compression_pool() -> std::sync::Arc<rayon::ThreadPool> {
    compression_pool_for(default_compression_threads())
}

#[cfg(feature = "parallel")]
fn build_compression_pool(threads: usize) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("rar5-compress-{i}"))
        .build()
        .expect("build rar5 compression pool")
}

/// Rayon pool for parallel extraction, sized with
/// [`set_extraction_threads`] (default: all cores). Rebuilt when the
/// requested count changes, like the compression pool.
#[cfg(feature = "parallel")]
pub(crate) fn extraction_pool() -> std::sync::Arc<rayon::ThreadPool> {
    use std::sync::{OnceLock, RwLock};
    static POOL: OnceLock<RwLock<std::sync::Arc<rayon::ThreadPool>>> = OnceLock::new();
    let lock = POOL.get_or_init(|| RwLock::new(std::sync::Arc::new(build_extraction_pool())));
    let current = lock.read().expect("pool lock").clone();
    let want = configured_extraction_threads().unwrap_or_else(|| pool_threads(4));
    if current.current_num_threads() != want {
        let mut guard = lock.write().expect("pool lock");
        if guard.current_num_threads() != want {
            *guard = std::sync::Arc::new(build_extraction_pool());
        }
        return guard.clone();
    }
    current
}

#[cfg(feature = "parallel")]
fn build_extraction_pool() -> rayon::ThreadPool {
    let threads = configured_extraction_threads().unwrap_or_else(|| pool_threads(4));
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("rar5-extract-{i}"))
        .build()
        .expect("build rar5 extraction pool")
}

// Set while a Rayon worker is preparing batch members. Nested parallelism
// (filter candidate probing, BLAKE2sp leaves) is disabled for small members
// on these threads: workers already parallelize across members, and nested
// tasks oversubscribe the pool.
#[cfg(feature = "parallel")]
thread_local! {
    static IN_BATCH_WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(feature = "parallel")]
pub(crate) fn in_batch_worker() -> bool {
    IN_BATCH_WORKER.with(|flag| flag.get())
}

#[cfg(feature = "parallel")]
pub(crate) struct BatchWorkerGuard;

#[cfg(feature = "parallel")]
impl BatchWorkerGuard {
    pub(crate) fn new() -> Self {
        IN_BATCH_WORKER.with(|flag| flag.set(true));
        BatchWorkerGuard
    }
}

#[cfg(feature = "parallel")]
impl Drop for BatchWorkerGuard {
    fn drop(&mut self) {
        IN_BATCH_WORKER.with(|flag| flag.set(false));
    }
}

#[cfg(all(test, feature = "parallel", not(target_family = "wasm")))]
mod tests {
    use super::*;

    #[test]
    fn default_compression_uses_all_host_cores() {
        // No per-process override set: the default pool must size itself to
        // every available host core (maximum parallelism), never the old
        // hard-capped 4.
        let want = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        assert!(
            want >= 2,
            "test host unexpectedly reports < 2 cores: {want}"
        );
        assert_eq!(
            default_compression_threads(),
            want,
            "compression did not default to all host cores"
        );
        // Extraction default must also be all cores on native.
        assert_eq!(
            configured_extraction_threads(),
            None,
            "unexpected global extraction override in test"
        );
    }
}

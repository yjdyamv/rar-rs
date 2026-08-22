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

/// Dedicated Rayon pool for batch compression.
///
/// The global pool (16 threads on this class of machine) makes many small
/// members *slower*: per-task allocation contention and SMT scheduling
/// overhead dominate tiny jobs. A small dedicated pool (at most 4 threads,
/// fewer on low-core machines) keeps the parallel win for medium/large
/// members without the small-member regression.
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
    // UV_THREADPOOL_SIZE > `default`. The extension sets SA_RAR5_WASM_WORKERS
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

/// Dedicated Rayon pool for batch compression.
#[cfg(feature = "parallel")]
pub(crate) fn compression_pool() -> &'static rayon::ThreadPool {
    use std::sync::OnceLock;

    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = configured_threads().unwrap_or_else(|| pool_threads(4).min(4));
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("rar5-compress-{i}"))
            .build()
            .expect("build rar5 compression pool")
    })
}

/// Rayon pool for parallel extraction, sized with
/// [`set_extraction_threads`] (default: all cores).
#[cfg(feature = "parallel")]
pub(crate) fn extraction_pool() -> &'static rayon::ThreadPool {
    use std::sync::OnceLock;

    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = configured_extraction_threads().unwrap_or_else(|| pool_threads(4));
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("rar5-extract-{i}"))
            .build()
            .expect("build rar5 extraction pool")
    })
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

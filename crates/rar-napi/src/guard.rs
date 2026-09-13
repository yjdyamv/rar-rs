//! Panic firewall for the task worker threads.
//!
//! napi-rs runs [`napi::Task::compute`] on a worker thread with no
//! `catch_unwind` around it, so a panic unwinds into the C callback and
//! aborts the whole Node process (and traps unrecoverably on
//! `wasm32-wasip1-threads`). Every task entry point runs its fallible work
//! through [`run_guarded`], which converts a panic into a binding error.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};

use napi::bindgen_prelude::{Error, Result, Status};

/// Run `f`, turning any panic into a [`Status::GenericFailure`] error.
pub(crate) fn run_guarded<F, T>(f: F) -> Result<T>
where
  F: FnOnce() -> Result<T>,
{
  match catch_unwind(AssertUnwindSafe(|| {
    inject_test_panic();
    f()
  })) {
    Ok(result) => result,
    Err(payload) => Err(panic_error(payload.as_ref())),
  }
}

fn panic_error(payload: &(dyn Any + Send)) -> Error {
  let detail = payload
    .downcast_ref::<&str>()
    .map(|message| (*message).to_string())
    .or_else(|| payload.downcast_ref::<String>().cloned())
    .unwrap_or_else(|| "non-string panic payload".to_string());
  Error::new(Status::GenericFailure, format!("internal panic: {detail}"))
}

/// Test-only panic injection seam: when `RAR_RS_NAPI_TEST_PANIC` is set,
/// guarded work panics before it starts, so tests can assert the firewall
/// maps the panic to an error instead of unwinding.
#[cfg(test)]
fn inject_test_panic() {
  if let Ok(message) = std::env::var("RAR_RS_NAPI_TEST_PANIC") {
    panic!("{message}");
  }
}

#[cfg(not(test))]
fn inject_test_panic() {}

/// Serializes tests that exercise guarded entry points: the panic seam is
/// process-global (an environment variable), so concurrent tests would
/// observe each other's injected panics.
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
  TEST_LOCK
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
  use super::{run_guarded, test_lock};
  use napi::Status;

  #[test]
  fn results_pass_through_unchanged() {
    let _lock = test_lock();
    assert_eq!(run_guarded(|| Ok(7u32)).unwrap(), 7);
    let error =
      run_guarded(|| -> napi::Result<u32> { Err(napi::Error::new(Status::InvalidArg, "bad")) })
        .unwrap_err();
    assert_eq!(error.status, Status::InvalidArg);
    assert!(error.reason.contains("bad"));
  }

  #[test]
  fn closure_panics_become_binding_errors() {
    let _lock = test_lock();
    let error = run_guarded(|| -> napi::Result<u32> { panic!("closure panic") }).unwrap_err();
    assert_eq!(error.status, Status::GenericFailure);
    assert!(error.reason.contains("internal panic"), "{}", error.reason);
    assert!(error.reason.contains("closure panic"), "{}", error.reason);
  }
}

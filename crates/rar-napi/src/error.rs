//! Stable N-API error mapping: the rar-rs `RarError` categories to N-API
//! status codes, plus the machine-readable code the JS wrapper rehydrates.
//!
//! N-API has no way to attach a custom property to a thrown `Error`: napi-rs
//! builds every one through `napi_create_error`, whose only configurable
//! field is the `code` property, and it locks that to the status string
//! (`env.create_error` in napi-rs: `napi_create_error(env, error_code,
//! reason, ...)` with `error_code = status`). So the stable library
//! [`ErrorCode`](rar_rs::ErrorCode) rides inside the message behind
//! [`CODE_MARKER`], and `rar-rs.js` parses it back out into a real `RarError`
//! with `code`, `rarCode` and a clean `message`.
//!
//! Keep [`CODE_MARKER`] and the code strings in sync with `rar-rs.js` and the
//! `RarErrorCode` union in the checked-in `rar-rs.d.ts`. Neither may be
//! renamed to a name `napi build` generates: see the naming rule in
//! `docs/PITFALLS.md`.

use napi::bindgen_prelude::*;

/// Prefixes the stable machine-readable code inside an error message.
///
/// Chosen so it cannot be produced by an archive or file name: a message is
/// either `"<marker><code>] <text>"` or an internal error with no marker.
pub(crate) const CODE_MARKER: &str = "[rar-rs:";

/// Every string [`rar_rs::ErrorCode::as_str`] can return, plus `"internal"`
/// for errors that have no library category (a panic caught by the firewall).
///
/// Test-only: its job is to keep the mapping covered when a category is added,
/// and to mirror the `RarErrorCode` union the JS entry point publishes.
#[cfg(test)]
pub(crate) const STABLE_CODES: [&str; 18] = [
  "format",
  "invalid_state",
  "invalid_option",
  "crc_mismatch",
  "hash_mismatch",
  "encrypted",
  "unsupported",
  "security",
  "limit_exceeded",
  "member_not_found",
  "ambiguous_member",
  "stale_entry_id",
  "archive_locked",
  "cancelled",
  "wrong_password",
  "create",
  "io",
  "internal",
];

/// The code used for a failure that never reached the library (a panic).
pub(crate) const INTERNAL_CODE: &str = "internal";

/// Compose the message the JS wrapper parses: `"[rar-rs:<code>] <text>"`.
pub(crate) fn message_with_code(code: &str, message: impl std::fmt::Display) -> String {
  format!("{CODE_MARKER}{code}] {message}")
}

/// Map a library error onto an N-API status.
///
/// The status is a *coarse* JS-visible classification; the precise category
/// travels in the message (see the module docs). It is chosen so the common
/// failures land on the status a JS caller would branch on anyway:
///
/// - a missing member is a `NotFound`-shaped failure, not a bad argument;
/// - a wrong password / CRC / hash / lock is a state the caller can act on,
///   which is `GenericFailure`'s only remaining bucket;
/// - `Cancelled` maps to `Status::Cancelled`, which is what `AbortSignal`
///   rejection paths check.
fn status_for(code: rar_rs::ErrorCode) -> Status {
  use rar_rs::ErrorCode;
  match code {
    // Genuinely invalid arguments or unsupported option combinations.
    ErrorCode::InvalidOption | ErrorCode::Unsupported => Status::InvalidArg,
    ErrorCode::MemberNotFound | ErrorCode::AmbiguousMember => Status::InvalidArg,
    // Everything else is an operation outcome, not a bad argument.
    ErrorCode::Format
    | ErrorCode::InvalidState
    | ErrorCode::CrcMismatch
    | ErrorCode::HashMismatch
    | ErrorCode::Encrypted
    | ErrorCode::Security
    | ErrorCode::LimitExceeded
    | ErrorCode::StaleEntryId
    | ErrorCode::ArchiveLocked
    | ErrorCode::WrongPassword
    | ErrorCode::Create
    | ErrorCode::Io => Status::GenericFailure,
    ErrorCode::Cancelled => Status::Cancelled,
    // `ErrorCode` is `#[non_exhaustive]`: a future category falls back to a
    // generic failure until it gets a deliberate status here.
    _ => Status::GenericFailure,
  }
}

/// Convert a library error into the binding error thrown to JS.
pub(crate) fn to_napi_error(err: rar_rs::RarError) -> Error {
  let code = err.code().as_str();
  Error::new(status_for(err.code()), message_with_code(code, err))
}

#[cfg(test)]
mod tests {
  use super::{INTERNAL_CODE, STABLE_CODES, message_with_code, to_napi_error};
  use napi::Status;

  /// A `RarError` for every `ErrorCode` the library can report, so the code
  /// list and the mapping stay covered when a category is added.
  fn one_of_each() -> Vec<(rar_rs::RarError, &'static str, Status)> {
    use rar_rs::RarError as E;
    vec![
      (E::format("bad header"), "format", Status::GenericFailure),
      (
        E::invalid_state("read mode"),
        "invalid_state",
        Status::GenericFailure,
      ),
      (
        E::invalid_option("bad option"),
        "invalid_option",
        Status::InvalidArg,
      ),
      (
        E::crc(1, 2, "member"),
        "crc_mismatch",
        Status::GenericFailure,
      ),
      (
        E::hash_mismatch([1; 32], [2; 32], "member"),
        "hash_mismatch",
        Status::GenericFailure,
      ),
      (
        E::encrypted("password required"),
        "encrypted",
        Status::GenericFailure,
      ),
      (E::unsupported("feature"), "unsupported", Status::InvalidArg),
      (
        E::security("unsafe path"),
        "security",
        Status::GenericFailure,
      ),
      (
        E::limit_exceeded(1, "test"),
        "limit_exceeded",
        Status::GenericFailure,
      ),
      (
        E::member_not_found("x"),
        "member_not_found",
        Status::InvalidArg,
      ),
      (
        E::ambiguous_member("x", 2),
        "ambiguous_member",
        Status::InvalidArg,
      ),
      (E::StaleEntryId, "stale_entry_id", Status::GenericFailure),
      (E::ArchiveLocked, "archive_locked", Status::GenericFailure),
      (E::Cancelled, "cancelled", Status::Cancelled),
      (E::WrongPassword, "wrong_password", Status::GenericFailure),
      (
        E::create(std::path::Path::new("out/a.bin"), "dir in the way"),
        "create",
        Status::GenericFailure,
      ),
      (
        E::Io(std::io::Error::other("disk")),
        "io",
        Status::GenericFailure,
      ),
    ]
  }

  #[test]
  fn every_library_category_keeps_its_code_and_status() {
    for (err, expected_code, expected_status) in one_of_each() {
      let error = to_napi_error(err);
      assert_eq!(error.status, expected_status, "status for {expected_code}");
      let expected_prefix = format!("[rar-rs:{expected_code}] ");
      assert!(
        error.reason.starts_with(&expected_prefix),
        "reason {:?} must start with {expected_prefix:?}",
        error.reason
      );
    }
  }

  #[test]
  fn the_covered_codes_match_the_declared_allowlist() {
    // Guards the JS parser: every code a message can carry must be listed in
    // `STABLE_CODES` (and therefore in the `RarErrorCode` TS union).
    for (err, expected_code, _) in one_of_each() {
      assert!(
        STABLE_CODES.contains(&expected_code),
        "{expected_code} is missing from STABLE_CODES"
      );
      assert_eq!(
        err.code().as_str(),
        expected_code,
        "the library renamed a category"
      );
    }
    assert!(STABLE_CODES.contains(&INTERNAL_CODE));
  }

  #[test]
  fn message_shape_is_marker_code_bracket_text() {
    assert_eq!(
      message_with_code("io", "boom"),
      "[rar-rs:io] boom".to_string()
    );
  }
}

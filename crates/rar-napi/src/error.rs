//! Stable N-API error mapping: rar5 `RarError` variants to N-API status
//! codes and messages.

use napi::bindgen_prelude::*;

pub(crate) fn to_napi_error(err: rar5::RarError) -> Error {
  // napi-rs tasks expose N-API Status strings as JS `error.code`. N-API has
  // no dedicated unsupported/security/not-found statuses, so keep a stable
  // semantic split instead of assigning unrelated status names.
  let status = match &err {
    rar5::RarError::Format(_)
    | rar5::RarError::InvalidOption(_)
    | rar5::RarError::Encrypted(_)
    | rar5::RarError::Security(_)
    | rar5::RarError::LimitExceeded { .. }
    | rar5::RarError::MemberNotFound { .. }
    | rar5::RarError::AmbiguousMember { .. }
    | rar5::RarError::StaleEntryId
    | rar5::RarError::WrongPassword => Status::InvalidArg,
    rar5::RarError::Cancelled => Status::Cancelled,
    rar5::RarError::InvalidState(_)
    | rar5::RarError::Crc { .. }
    | rar5::RarError::HashMismatch { .. }
    | rar5::RarError::Unsupported(_)
    | rar5::RarError::ArchiveLocked
    | rar5::RarError::Io(_) => Status::GenericFailure,
    _ => Status::GenericFailure,
  };
  Error::new(status, format!("rar: {err}"))
}

#[cfg(test)]
mod tests {
  use super::to_napi_error;
  use napi::Status;

  #[test]
  fn core_error_variants_have_stable_napi_statuses() {
    let invalid_arguments = [
      rar5::RarError::Format("bad header".into()),
      rar5::RarError::InvalidOption("bad option".into()),
      rar5::RarError::Encrypted("password required".into()),
      rar5::RarError::Security("unsafe path".into()),
      rar5::RarError::LimitExceeded {
        limit: 1,
        context: "test".into(),
      },
      rar5::RarError::MemberNotFound { name: "x".into() },
      rar5::RarError::AmbiguousMember {
        name: "x".into(),
        matches: 2,
      },
      rar5::RarError::StaleEntryId,
      rar5::RarError::WrongPassword,
    ];
    for err in invalid_arguments {
      assert_eq!(to_napi_error(err).status, Status::InvalidArg);
    }

    let operation_failures = [
      rar5::RarError::InvalidState("read mode".into()),
      rar5::RarError::Unsupported("feature".into()),
      rar5::RarError::ArchiveLocked,
      rar5::RarError::Crc {
        expected: 1,
        actual: 2,
        context: "member".into(),
      },
      rar5::RarError::HashMismatch {
        expected: [1; 32],
        actual: [2; 32],
        context: "member".into(),
      },
      rar5::RarError::Io(std::io::Error::other("disk")),
    ];
    for err in operation_failures {
      assert_eq!(to_napi_error(err).status, Status::GenericFailure);
    }

    assert_eq!(
      to_napi_error(rar5::RarError::Cancelled).status,
      Status::Cancelled
    );
  }
}

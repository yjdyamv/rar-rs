//! Stable N-API error mapping: rar5 `RarError` variants to N-API status
//! codes and messages.

use napi::bindgen_prelude::*;

pub(crate) fn to_napi_error(err: rar_rs::RarError) -> Error {
  // napi-rs tasks expose N-API Status strings as JS `error.code`. N-API has
  // no dedicated unsupported/security/not-found statuses, so keep a stable
  // semantic split instead of assigning unrelated status names.
  let status = match &err {
    rar_rs::RarError::Format(_)
    | rar_rs::RarError::InvalidOption(_)
    | rar_rs::RarError::Encrypted(_)
    | rar_rs::RarError::Security(_)
    | rar_rs::RarError::LimitExceeded { .. }
    | rar_rs::RarError::MemberNotFound { .. }
    | rar_rs::RarError::AmbiguousMember { .. }
    | rar_rs::RarError::StaleEntryId
    | rar_rs::RarError::WrongPassword => Status::InvalidArg,
    rar_rs::RarError::Cancelled => Status::Cancelled,
    rar_rs::RarError::InvalidState(_)
    | rar_rs::RarError::Crc { .. }
    | rar_rs::RarError::HashMismatch { .. }
    | rar_rs::RarError::Unsupported(_)
    | rar_rs::RarError::ArchiveLocked
    | rar_rs::RarError::Io(_) => Status::GenericFailure,
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
      rar_rs::RarError::Format("bad header".into()),
      rar_rs::RarError::InvalidOption("bad option".into()),
      rar_rs::RarError::Encrypted("password required".into()),
      rar_rs::RarError::Security("unsafe path".into()),
      rar_rs::RarError::LimitExceeded {
        limit: 1,
        context: "test".into(),
      },
      rar_rs::RarError::MemberNotFound { name: "x".into() },
      rar_rs::RarError::AmbiguousMember {
        name: "x".into(),
        matches: 2,
      },
      rar_rs::RarError::StaleEntryId,
      rar_rs::RarError::WrongPassword,
    ];
    for err in invalid_arguments {
      assert_eq!(to_napi_error(err).status, Status::InvalidArg);
    }

    let operation_failures = [
      rar_rs::RarError::InvalidState("read mode".into()),
      rar_rs::RarError::Unsupported("feature".into()),
      rar_rs::RarError::ArchiveLocked,
      rar_rs::RarError::Crc {
        expected: 1,
        actual: 2,
        context: "member".into(),
      },
      rar_rs::RarError::HashMismatch {
        expected: [1; 32],
        actual: [2; 32],
        context: "member".into(),
      },
      rar_rs::RarError::Io(std::io::Error::other("disk")),
    ];
    for err in operation_failures {
      assert_eq!(to_napi_error(err).status, Status::GenericFailure);
    }

    assert_eq!(
      to_napi_error(rar_rs::RarError::Cancelled).status,
      Status::Cancelled
    );
  }
}

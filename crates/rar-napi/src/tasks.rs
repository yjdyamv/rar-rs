//! Async task glue: napi `Task` implementations and the factory functions
//! that build them. (Probe slice; the remaining tasks migrate here next.)

use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::error::to_napi_error;

/// Read one member's uncompressed content into memory (like previewing a
/// file inside the archive). Bounded by the library's default 4 GiB
/// per-member read limit; use `extractArchive` for arbitrarily large
/// members.
#[napi(ts_return_type = "Promise<Buffer>")]
pub fn read_member(
  archive_path: String,
  name: String,
  password: Option<String>,
) -> AsyncTask<ReadMemberTask> {
  AsyncTask::new(ReadMemberTask {
    archive_path,
    name,
    password,
  })
}

pub struct ReadMemberTask {
  archive_path: String,
  name: String,
  password: Option<String>,
}

#[napi]
impl Task for ReadMemberTask {
  type Output = Vec<u8>;
  type JsValue = Buffer;

  fn compute(&mut self) -> Result<Self::Output> {
    let mut archive = match self.password.as_deref() {
      Some(pw) if !pw.is_empty() => {
        rar5::RarArchive::open_with_password(&self.archive_path, pw).map_err(to_napi_error)?
      }
      _ => rar5::RarArchive::open(&self.archive_path).map_err(to_napi_error)?,
    };
    archive.read(&self.name).map_err(to_napi_error)
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output.into())
  }
}

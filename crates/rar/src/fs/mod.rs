//! Filesystem policy shared across archive roles: atomic staging and
//! replacement, volume naming/discovery helpers, and safe-path handling
//! for extraction. Format modules consume these policies instead of
//! defining their own.

pub(crate) mod atomic;
pub(crate) mod safe_path;
pub(crate) mod volume;

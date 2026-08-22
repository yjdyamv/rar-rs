//! Crypto layer: per-format-family encryption primitives.
//!
//! Mirrors the reference layout — `crypto/rar50.rs` holds the RAR5
//! AES-256-CBC stack (PBKDF2-style key derivation, hash-key MAC, streaming
//! CBC). Later format families add their own `crypto/rar*.rs` modules.

pub mod rar50;

pub use rar50::*;

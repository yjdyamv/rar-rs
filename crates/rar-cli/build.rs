//! Export the target triple to the crate's tests.
//!
//! The SFX tests have to build the console stub (`rar-sfx`) for the *same*
//! target as the test binary: CI runs the suite under
//! `--target i686-unknown-linux-gnu`, and cargo neither passes `--target`
//! down to a nested `cargo build` nor exports `TARGET` to test processes, so
//! `sfx_stub()` would otherwise build (and look for) a host-architecture stub.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=RAR_SFX_TARGET={target}");
}

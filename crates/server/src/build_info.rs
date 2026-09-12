//! Compiled-in build identity, the string `--version` prints.
//!
//! A release artifact has to answer two questions a running server cannot: which
//! commit produced this file, and which platform it was built for. Neither is
//! visible from `/api/v1/version`, which answers for a *running* server; this
//! answers for the file someone is holding, and it is the check a release
//! verification runs before it trusts a download.
//!
//! The two constants that are not in `Cargo.toml` are stamped by
//! `crates/server/build.rs`; the protocol version is the same constant the
//! server reports on the wire and the daemon refuses to connect without
//! (`docs/upgrades.md`), so `--version` is where an operator sees the mismatch
//! before the daemon reports it for them.

/// Crate version, from the workspace manifest.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Commit the binary was built from, or `unknown` when the build had no
/// repository and no explicit `LOOM_GIT_COMMIT`.
pub const COMMIT: &str = env!("LOOM_GIT_COMMIT");

/// Target triple the binary was compiled for, such as
/// `x86_64-unknown-linux-musl`.
pub const TARGET: &str = env!("LOOM_BUILD_TARGET");

/// The single line `<name> --version` prints.
///
/// One line, not several, so a pipeline can match it with a substring test
/// without a parser. The name is a parameter because the same identity is
/// embedded in two binaries that must not claim to be each other.
pub fn version_line(name: &str) -> String {
    format!(
        "{name} {VERSION} ({TARGET}, protocol {}, commit {COMMIT})",
        crate::PROTOCOL_VERSION
    )
}

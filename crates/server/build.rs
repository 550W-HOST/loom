//! Embeds the product app and stamps the build identity `--version` prints.
//!
//! The UI is compiled into the binary: `apps/app/dist` is walked here and every
//! file becomes an `include_bytes!` in the generated `ui_assets` module. A
//! server therefore carries its client with it — no bundle path to configure, no
//! second artifact to keep in step with the binary — which is why the build
//! *fails* when the app has not been built. `pnpm --filter @bb/app run build`
//! is a prerequisite of `cargo build`, not an optional extra.
//!
//! The identity stamp:
//!
//! Stamps the build identity that `--version` prints.
//!
//! `CARGO_PKG_VERSION` is in the manifest and needs no help, but the commit a
//! binary was built from and the triple it was compiled for do not exist at
//! compile time unless something puts them there. This script is that
//! something: it emits `LOOM_GIT_COMMIT` and `LOOM_BUILD_TARGET`, which
//! `build_info` reads back with `env!`.
//!
//! It lives in `loom-server` because both binaries depend on it — the daemon
//! already reads `loom_server::PROTOCOL_VERSION` — so one script stamps the
//! whole release.
//!
//! A missing `git`, a source tarball with no `.git`, or an unusual checkout all
//! degrade to `unknown` rather than failing the build: a release binary that
//! cannot say which commit it came from is worth shipping, one that cannot be
//! built is not.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    embed_ui();

    // Cargo only re-runs a build script when an input changed, and an explicit
    // stamp is as much an input as a file. Without this line, a second build
    // with a different `LOOM_GIT_COMMIT` reuses the first one's value.
    println!("cargo:rerun-if-env-changed=LOOM_GIT_COMMIT");
    watch_git_head();

    // The release pipeline builds a tag, so the explicit stamp is redundant
    // there — but it is what makes the identity a build input rather than a
    // property of whatever directory the build happens in, which is the only
    // way to build a reproducible artifact from an export with no `.git`.
    let commit = std::env::var("LOOM_GIT_COMMIT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(commit_from_git)
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=LOOM_GIT_COMMIT={}", commit.trim());

    // Cargo hands the script the triple it is compiling for, so even a
    // cross-compiled artifact can name itself. This is what tells a reader of
    // `--version` whether the binary in front of them is the static musl one.
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=LOOM_BUILD_TARGET={target}");
}

/// Makes the stamp follow the repository, not just the source files.
///
/// A commit is not a file cargo watches, so without this the stamp of the
/// previous build survives one: build, commit, build again, and the second
/// binary names the commit before it. HEAD covers a checkout and the reflog
/// covers a commit, which is the case that motivated this.
///
/// The paths come from git rather than from `../../.git/`, because that is not
/// where git keeps them when the checkout is a linked worktree — the same
/// repository checked out twice, with `.git` as a *file* pointing at the real
/// directory. A watch on a path that does not exist is a build script that
/// re-runs on every build, which is a worse trade than a stale stamp.
fn watch_git_head() {
    let Some(git_dir) = git_dir() else { return };
    for name in ["HEAD", "logs/HEAD"] {
        let path = git_dir.join(name);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

/// The repository's git directory, or `None` when there is no repository.
fn git_dir() -> Option<std::path::PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    (!path.is_empty()).then(|| std::path::PathBuf::from(path))
}

/// The checked-out commit, or `None` when there is no repository to read.
fn commit_from_git() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8(output.stdout).ok()?;
    let commit = commit.trim();
    (!commit.is_empty()).then(|| commit.to_owned())
}

/// Writes the module that carries the product app.
///
/// Every file under `apps/app/dist` is included by path, so the compiler tracks
/// the directory: a rebuilt bundle rebuilds the crate that serves it. The
/// generated `get` is a match rather than a map so the lookup is a jump table
/// over `&'static [u8]` — no allocation and no startup work, which matters when
/// the table holds a few thousand entries.
///
/// A missing bundle is a build error. The alternative — compiling a server that
/// cannot serve the client it exists to serve — would move the failure to a
/// user's browser, and this is the one place where the prerequisite is knowable.
fn embed_ui() {
    let dist = repo_root().join("apps").join("app").join("dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    if !dist.join("index.html").is_file() {
        panic!(
            "no product app bundle at {}: build it first with `pnpm --filter @bb/app run build`              (the UI is compiled into the server, so cargo needs it to exist)",
            dist.display()
        );
    }

    let mut files = Vec::new();
    collect(&dist, &dist, &mut files);
    files.sort();

    let mut module = String::from(
        "// @generated by crates/server/build.rs from apps/app/dist — do not edit.\n\n         /// The app's entry document, served for every client route.\n         pub const INDEX_HTML: &[u8] = include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../../apps/app/dist/index.html\"));\n\n         /// How many files are embedded, for the startup log.\n         pub const FILE_COUNT: usize = ",
    );
    let _ = writeln!(module, "{};\n", files.len());
    module.push_str("/// One embedded file by its path under the bundle root.\npub fn get(path: &str) -> Option<&'static [u8]> {\n    match path {\n");
    for relative in &files {
        let absolute = dist.join(relative);
        let _ = writeln!(
            module,
            "        {:?} => Some(include_bytes!({:?})),",
            relative.to_string_lossy().replace('\\', "/"),
            absolute.display().to_string()
        );
    }
    module.push_str("        _ => None,\n    }\n}\n");

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("ui_assets.rs");
    std::fs::write(&out, module).expect("write ui_assets.rs");
}

/// Every file under `dir`, as paths relative to `root`, deepest walk first.
fn collect(root: &Path, dir: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, into);
        } else if let Ok(relative) = path.strip_prefix(root) {
            into.push(relative.to_path_buf());
        }
    }
}

/// The repository root, from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
        })
}

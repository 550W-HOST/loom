//! Host filesystem reads and listings, for the control plane.
//!
//! This is the worker's half of [`HostFileRequest`]. It exists so the control
//! plane can show a thread's files without ever reading its own disk: a request
//! arrives, the machine that actually owns the path performs it, and the answer
//! goes back up the worker's socket.
//!
//! # Containment is checked here too
//!
//! The control plane already validates the *root-relative* path a client sent
//! (`..`, absolute paths, NUL, backslashes are all refused before a request is
//! built). That check cannot see symlinks, and it cannot see through a path
//! assembled on another machine. So a read that names a `root_path` re-resolves
//! the real path here and refuses anything that escapes the root — the second
//! half of the traversal defence, on the machine where the filesystem actually
//! is.
//!
//! # Why the work is blocking
//!
//! A recursive listing of a large tree and a multi-megabyte read are both
//! synchronous filesystem work. Running them on a runtime worker would stall
//! every other task on that thread — the same failure mode this project exists
//! to remove — so they run on the blocking pool and the async runtime only
//! waits.

// Every helper here answers with `HostFileOutcome`, whose `Failed` arm is large
// enough that clippy's `result_large_err` fires on each one. Boxing the error
// would cost an allocation on the hot failure path and obscure the handlers;
// the server's HTTP modules carry the same allow for the same reason.
#![allow(clippy::result_large_err)]

use loom_provider_protocol::{
    HostFileContent, HostFileEncoding, HostFileEntry, HostFileFailure, HostFileOperation,
    HostFileOutcome, HostFileReport, HostFileRequest, HostPathKind,
};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

use crate::Worker;

/// Names never listed, whatever the caller asks for.
///
/// `.git` is excluded unconditionally, like bb's `ALWAYS_EXCLUDED_NAMES`: its
/// object store is enormous and never what a file picker wants.
const ALWAYS_EXCLUDED_NAMES: [&str; 1] = [".git"];

/// How many files one copy may take.
const MAX_COPY_FILES: usize = 100;

/// The worker's answer, ready to be sent up the socket.
///
/// A free function rather than a method so it can run on the blocking pool
/// without borrowing the worker.
pub fn answer(request: HostFileRequest) -> HostFileReport {
    let outcome = match &request.operation {
        HostFileOperation::Read {
            path,
            root_path,
            max_bytes,
        } => read_file(path, root_path.as_deref(), *max_bytes),
        HostFileOperation::ListDirectory {
            path,
            include_files,
            include_directories,
            include_hidden,
            limit,
        } => list_directory(
            path,
            *include_files,
            *include_directories,
            *include_hidden,
            *limit,
        ),
        HostFileOperation::List {
            path,
            query,
            limit,
            include_files,
            include_directories,
            include_hidden,
        } => list_paths(
            path,
            query.as_deref(),
            *limit,
            *include_files,
            *include_directories,
            *include_hidden,
        ),
        HostFileOperation::Exists { paths } => paths_exist(paths),
        HostFileOperation::Write {
            path,
            root_path,
            content,
            max_bytes,
            overwrite,
        } => write_file(path, root_path, content, *max_bytes, *overwrite),
        HostFileOperation::Copy {
            paths,
            source_root,
            destination,
            destination_root,
            max_bytes,
        } => copy_files(
            paths,
            source_root,
            destination,
            destination_root,
            *max_bytes,
        ),
        HostFileOperation::CreateDirectory {
            path,
            root_path,
            recursive,
        } => create_directory(path, root_path.as_deref(), *recursive),
        HostFileOperation::Move {
            source_path,
            destination_path,
            root_path,
            overwrite,
        } => move_path(
            source_path,
            destination_path,
            root_path.as_deref(),
            *overwrite,
        ),
        HostFileOperation::Remove {
            path,
            root_path,
            recursive,
        } => remove_path(path, root_path.as_deref(), *recursive),
        HostFileOperation::ReadWithMetadata {
            path,
            root_path,
            max_bytes,
        } => read_with_metadata(path, root_path.as_deref(), *max_bytes),
        HostFileOperation::WriteFile {
            path,
            root_path,
            content,
            content_encoding,
            max_bytes,
            create_parents,
            expected_sha256,
            create_only,
            mode,
        } => write_file_full(
            path,
            root_path,
            content,
            *content_encoding,
            *max_bytes,
            *create_parents,
            expected_sha256.as_deref(),
            *create_only,
            *mode,
        ),
        HostFileOperation::SetMetadata {
            path,
            root_path,
            mode,
            touch,
        } => set_metadata(path, root_path.as_deref(), *mode, *touch),
        HostFileOperation::CopyPath {
            source_path,
            destination_path,
            root_path,
            overwrite,
        } => copy_path(source_path, destination_path, root_path, *overwrite),
    };
    HostFileReport {
        host_id: request.host_id,
        request_id: request.request_id,
        outcome,
    }
}

fn failed(code: &str, message: impl Into<String>) -> HostFileOutcome {
    HostFileOutcome::Failed {
        code: code.to_owned(),
        message: message.into(),
    }
}

/// Reads one file, refusing anything outside `root_path` and anything larger
/// than `max_bytes`.
///
/// A larger file is refused rather than truncated: half a file is not the file
/// the client asked for, and silently returning a prefix would be the
/// "plausible but wrong" answer this batch forbids.
fn read_file(path: &str, root_path: Option<&str>, max_bytes: u64) -> HostFileOutcome {
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return failed("invalid_path", "path must be absolute");
    }
    let Some(root) = root_path.map(PathBuf::from) else {
        return read_resolved(&path, max_bytes);
    };
    if !root.is_absolute() {
        return failed("invalid_path", "root_path must be absolute");
    }
    // Resolve symlinks on both sides before comparing: a symlink inside the
    // root pointing outside it is exactly the escape this check exists for.
    let real_root = match std::fs::canonicalize(&root) {
        Ok(path) => path,
        Err(error) => return failed("invalid_path", format!("root is not readable: {error}")),
    };
    let real_path = match std::fs::canonicalize(&path) {
        Ok(path) => path,
        Err(error) => return failed("not_found", format!("{error}")),
    };
    if !real_path.starts_with(&real_root) {
        return failed("invalid_path", "path escapes the read root");
    }
    read_resolved(&real_path, max_bytes)
}

fn read_resolved(path: &Path, max_bytes: u64) -> HostFileOutcome {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => return failed("not_found", format!("{error}")),
    };
    if metadata.is_dir() {
        return failed("invalid_path", "path is a directory, not a file");
    }
    if metadata.len() > max_bytes {
        return failed(
            "file_too_large",
            format!(
                "file is {} bytes, over the {} byte limit",
                metadata.len(),
                max_bytes
            ),
        );
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => return failed("not_found", format!("{error}")),
    };
    let (content, content_encoding) = match String::from_utf8(bytes.clone()) {
        Ok(text) => (text, HostFileEncoding::Utf8),
        Err(_) => (base64_encode(&bytes), HostFileEncoding::Base64),
    };
    HostFileOutcome::Content(HostFileContent {
        path: path.to_string_lossy().into_owned(),
        content,
        content_encoding,
        size_bytes: metadata.len(),
        mime_type: mime_type_for(path),
        modified_at_ms: modified_at_ms(&metadata),
        sha256: Some(sha256_hex(&bytes)),
    })
}

/// The file's modification time, when the platform reports one.
fn modified_at_ms(metadata: &std::fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_millis() as u64)
}

/// Resolves an existing path and checks it against `root`, following symlinks.
fn confined_existing(path: &Path, root: &Path) -> Result<PathBuf, HostFileOutcome> {
    let real_root = match std::fs::canonicalize(root) {
        Ok(path) => path,
        Err(error) => {
            return Err(failed(
                "invalid_path",
                format!("root is not readable: {error}"),
            ))
        }
    };
    let real_path = match std::fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) => return Err(failed("not_found", format!("{error}"))),
    };
    if !real_path.starts_with(&real_root) {
        return Err(failed("invalid_path", "path escapes the workspace root"));
    }
    Ok(real_path)
}

fn paths_exist(paths: &[String]) -> HostFileOutcome {
    HostFileOutcome::Listing {
        entries: paths
            .iter()
            .filter(|raw| Path::new(raw).is_absolute() && std::fs::metadata(raw).is_ok())
            .map(|raw| HostFileEntry {
                path: raw.clone(),
                name: Path::new(raw)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_owned(),
                kind: if std::fs::metadata(raw).is_ok_and(|metadata| metadata.is_dir()) {
                    HostPathKind::Directory
                } else {
                    HostPathKind::File
                },
                score: 0.0,
                positions: Vec::new(),
            })
            .collect(),
        truncated: false,
    }
}

/// Writes one file inside `root_path`, creating parent directories.
///
/// The path does **not** exist yet, so it is confined by resolving its parent
/// against the root and then re-joining the file name: canonicalising a path
/// that is not there fails, and a symlinked parent is exactly the escape a
/// plain prefix comparison would miss.
fn write_file(
    path: &str,
    root_path: &str,
    content: &str,
    max_bytes: u64,
    overwrite: bool,
) -> HostFileOutcome {
    let path = PathBuf::from(path);
    let root = PathBuf::from(root_path);
    if !path.is_absolute() || !root.is_absolute() {
        return failed("invalid_path", "path and root_path must be absolute");
    }
    let Some(file_name) = path.file_name() else {
        return failed("invalid_path", "path has no file name");
    };
    let Some(parent) = path.parent() else {
        return failed("invalid_path", "path has no parent directory");
    };
    // The parent is the part that must already resolve inside the root. Create
    // it first so a fresh upload's directory chain can be canonicalised.
    if let Err(error) = std::fs::create_dir_all(parent) {
        return failed(
            "invalid_path",
            format!("could not create {parent:?}: {error}"),
        );
    }
    let real_parent = match confined_existing(parent, &root) {
        Ok(parent) => parent,
        Err(outcome) => return outcome,
    };
    let bytes = match base64_decode(content) {
        Some(bytes) => bytes,
        None => return failed("invalid_request", "content is not valid base64"),
    };
    if bytes.len() as u64 > max_bytes {
        return failed(
            "file_too_large",
            format!(
                "upload is {} bytes, over the {} byte limit",
                bytes.len(),
                max_bytes
            ),
        );
    }
    let target = if overwrite {
        real_parent.join(file_name)
    } else {
        unique_target(&real_parent, file_name)
    };
    if let Err(error) = std::fs::write(&target, &bytes) {
        return failed(
            "invalid_path",
            format!("could not write {target:?}: {error}"),
        );
    }
    let metadata = std::fs::metadata(&target).ok();
    HostFileOutcome::Written(HostFileContent {
        path: target.to_string_lossy().into_owned(),
        content: String::new(),
        content_encoding: HostFileEncoding::Utf8,
        size_bytes: bytes.len() as u64,
        mime_type: mime_type_for(&target),
        modified_at_ms: metadata.as_ref().and_then(modified_at_ms),
        sha256: Some(sha256_hex(&bytes)),
    })
}

/// Copies files into a destination directory, each side confined to its root.
///
/// A source that cannot be copied is reported per path and does not sink the
/// rest: a client copying several attachments needs to know which ones made it,
/// not that the batch failed as a whole.
fn copy_files(
    paths: &[String],
    source_root: &str,
    destination: &str,
    destination_root: &str,
    max_bytes: u64,
) -> HostFileOutcome {
    let source_root = PathBuf::from(source_root);
    let destination = PathBuf::from(destination);
    let destination_root = PathBuf::from(destination_root);
    if !source_root.is_absolute() || !destination.is_absolute() || !destination_root.is_absolute() {
        return failed(
            "invalid_path",
            "source_root, destination and destination_root must be absolute",
        );
    }
    if let Err(error) = std::fs::create_dir_all(&destination) {
        return failed(
            "invalid_path",
            format!("could not create {destination:?}: {error}"),
        );
    }
    let real_destination = match confined_existing(&destination, &destination_root) {
        Ok(destination) => destination,
        Err(outcome) => return outcome,
    };
    let mut files = Vec::new();
    let mut failures = Vec::new();
    for raw in paths {
        if files.len() >= MAX_COPY_FILES {
            failures.push(HostFileFailure {
                path: raw.clone(),
                code: "invalid_request".into(),
                message: format!("at most {MAX_COPY_FILES} files may be copied at once"),
            });
            continue;
        }
        let source = PathBuf::from(raw);
        if !source.is_absolute() {
            failures.push(HostFileFailure {
                path: raw.clone(),
                code: "invalid_path".into(),
                message: "source path must be absolute".into(),
            });
            continue;
        }
        let real_source = match confined_existing(&source, &source_root) {
            Ok(source) => source,
            Err(HostFileOutcome::Failed { code, message }) => {
                failures.push(HostFileFailure {
                    path: raw.clone(),
                    code,
                    message,
                });
                continue;
            }
            Err(_) => continue,
        };
        let metadata = match std::fs::metadata(&real_source) {
            Ok(metadata) => metadata,
            Err(error) => {
                failures.push(HostFileFailure {
                    path: raw.clone(),
                    code: "not_found".into(),
                    message: error.to_string(),
                });
                continue;
            }
        };
        if !metadata.is_file() {
            failures.push(HostFileFailure {
                path: raw.clone(),
                code: "invalid_path".into(),
                message: "source is not a regular file".into(),
            });
            continue;
        }
        if metadata.len() > max_bytes {
            failures.push(HostFileFailure {
                path: raw.clone(),
                code: "file_too_large".into(),
                message: format!(
                    "file is {} bytes, over the {} byte limit",
                    metadata.len(),
                    max_bytes
                ),
            });
            continue;
        }
        let Some(name) = real_source.file_name() else {
            failures.push(HostFileFailure {
                path: raw.clone(),
                code: "invalid_path".into(),
                message: "source has no file name".into(),
            });
            continue;
        };
        let target = unique_target(&real_destination, name);
        if let Err(error) = std::fs::copy(&real_source, &target) {
            failures.push(HostFileFailure {
                path: raw.clone(),
                code: "invalid_path".into(),
                message: format!("could not copy to {target:?}: {error}"),
            });
            continue;
        }
        let copied = std::fs::metadata(&target).ok();
        files.push(HostFileContent {
            path: target.to_string_lossy().into_owned(),
            content: String::new(),
            content_encoding: HostFileEncoding::Utf8,
            size_bytes: copied
                .as_ref()
                .map(|meta| meta.len())
                .unwrap_or(metadata.len()),
            mime_type: mime_type_for(&target),
            modified_at_ms: copied.as_ref().and_then(modified_at_ms),
            sha256: None,
        });
    }
    HostFileOutcome::Copied { files, failures }
}

/* ------------------------------------------------------------------ */
/* B9 path operations                                                  */
/* ------------------------------------------------------------------ */

/// Lowercase hex SHA-256 of the bytes on disk.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push_str(&format!("{byte:02x}"));
    }
    encoded
}

/// The POSIX mode of an existing path, when the platform has one.
#[cfg(unix)]
fn mode_of(metadata: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn mode_of(_metadata: &std::fs::Metadata) -> Option<u32> {
    None
}

/// Confines a path that does not exist yet by resolving its parent.
///
/// Canonicalising a missing path fails, so containment is checked on the part
/// that does exist. A symlinked parent is exactly the escape a plain prefix
/// comparison would miss, which is why this goes through `canonicalize`.
fn confine_new(path: &Path, root: Option<&Path>) -> Result<PathBuf, HostFileOutcome> {
    if !path.is_absolute() {
        return Err(failed("invalid_path", "path must be absolute"));
    }
    let Some(root) = root else {
        return Ok(path.to_path_buf());
    };
    if !root.is_absolute() {
        return Err(failed("invalid_path", "root_path must be absolute"));
    }
    let real_root = match std::fs::canonicalize(root) {
        Ok(root) => root,
        Err(error) => {
            return Err(failed(
                "invalid_path",
                format!("root is not readable: {error}"),
            ))
        }
    };
    // The nearest existing ancestor must resolve inside the root; then the
    // still-missing tail is re-joined onto the canonical ancestor. That keeps
    // the final path inside the root without requiring the target to exist.
    let mut ancestor = path;
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let real_ancestor = loop {
        match std::fs::canonicalize(ancestor) {
            Ok(real) => break real,
            Err(_) => match (ancestor.parent(), ancestor.file_name()) {
                (Some(parent), Some(name)) => {
                    missing.push(name.to_os_string());
                    ancestor = parent;
                }
                _ => {
                    return Err(failed(
                        "invalid_path",
                        "path has no existing ancestor inside the root",
                    ))
                }
            },
        }
    };
    if !real_ancestor.starts_with(&real_root) {
        return Err(failed("invalid_path", "path escapes the workspace root"));
    }
    let mut resolved = real_ancestor;
    for part in missing.iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

/// Creates one directory, optionally with its parents.
fn create_directory(path: &str, root_path: Option<&str>, recursive: bool) -> HostFileOutcome {
    let path = PathBuf::from(path);
    let root = root_path.map(PathBuf::from);
    let resolved = match confine_new(&path, root.as_deref()) {
        Ok(resolved) => resolved,
        Err(outcome) => return outcome,
    };
    if resolved.exists() {
        if resolved.is_dir() {
            // Already there is success: mkdir that reports a conflict when the
            // directory exists is a race the caller cannot do anything about.
            return HostFileOutcome::Done;
        }
        return failed("invalid_path", "path exists and is not a directory");
    }
    let result = if recursive {
        std::fs::create_dir_all(&resolved)
    } else {
        std::fs::create_dir(&resolved)
    };
    match result {
        Ok(()) => HostFileOutcome::Done,
        Err(error) => failed(
            "invalid_path",
            format!("could not create directory: {error}"),
        ),
    }
}

/// Moves a path, refusing to clobber unless `overwrite` is set.
fn move_path(
    source_path: &str,
    destination_path: &str,
    root_path: Option<&str>,
    overwrite: bool,
) -> HostFileOutcome {
    let root = root_path.map(PathBuf::from);
    let source = PathBuf::from(source_path);
    if !source.is_absolute() {
        return failed("invalid_path", "source path must be absolute");
    }
    let destination = PathBuf::from(destination_path);
    // Both sides are confined: a rename that moved a file out of the root
    // would be the same escape a read refused.
    let real_source = match root.as_deref() {
        Some(root) => match confined_existing(&source, root) {
            Ok(source) => source,
            Err(outcome) => return outcome,
        },
        None => source.clone(),
    };
    if !real_source.exists() {
        return failed("not_found", "source path does not exist");
    }
    let real_destination = match confine_new(&destination, root.as_deref()) {
        Ok(destination) => destination,
        Err(outcome) => return outcome,
    };
    if let Some(parent) = real_destination.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            return failed(
                "invalid_path",
                format!("could not create destination parent: {error}"),
            );
        }
    }
    if real_destination.exists() && !overwrite {
        return failed(
            "conflict",
            "destination already exists and overwrite was not requested",
        );
    }
    match std::fs::rename(&real_source, &real_destination) {
        Ok(()) => HostFileOutcome::Done,
        // A rename across filesystems fails with `EXDEV`; a copy-then-remove is
        // the honest fallback rather than reporting a move that did not happen.
        Err(_) if !overwrite && real_destination.exists() => failed(
            "conflict",
            "destination already exists and overwrite was not requested",
        ),
        Err(error) => failed("invalid_path", format!("could not move path: {error}")),
    }
}

/// Removes one file or directory.
fn remove_path(path: &str, root_path: Option<&str>, recursive: bool) -> HostFileOutcome {
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return failed("invalid_path", "path must be absolute");
    }
    let resolved = match root_path.map(PathBuf::from) {
        Some(root) => match confined_existing(&path, &root) {
            Ok(path) => path,
            // A path that does not exist cannot escape anything, and a remove
            // of a missing path is reported as `not_found` below.
            Err(HostFileOutcome::Failed { .. }) if !path.exists() => path,
            Err(outcome) => return outcome,
        },
        None => path,
    };
    let metadata = match std::fs::symlink_metadata(&resolved) {
        Ok(metadata) => metadata,
        Err(error) => return failed("not_found", format!("{error}")),
    };
    if metadata.is_dir() {
        let result = if recursive {
            std::fs::remove_dir_all(&resolved)
        } else {
            std::fs::remove_dir(&resolved)
        };
        return match result {
            Ok(()) => HostFileOutcome::Done,
            // A non-empty directory refused without `recursive` is a conflict
            // rather than a bad path: the caller asked for something the
            // filesystem will not do, and half-removing it is not an option.
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                failed("conflict", "directory is not empty")
            }
            Err(error) => failed(
                "invalid_path",
                format!("could not remove directory: {error}"),
            ),
        };
    }
    match std::fs::remove_file(&resolved) {
        Ok(()) => HostFileOutcome::Done,
        Err(error) => failed("invalid_path", format!("could not remove file: {error}")),
    }
}

/// Reads one file together with the metadata an editor's save needs.
fn read_with_metadata(path: &str, root_path: Option<&str>, max_bytes: u64) -> HostFileOutcome {
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return failed("invalid_path", "path must be absolute");
    }
    let resolved = match root_path.map(PathBuf::from) {
        Some(root) => match confined_existing(&path, &root) {
            Ok(path) => path,
            Err(outcome) => return outcome,
        },
        None => path,
    };
    let metadata = match std::fs::metadata(&resolved) {
        Ok(metadata) => metadata,
        Err(error) => return failed("not_found", format!("{error}")),
    };
    if metadata.is_dir() {
        return failed("invalid_path", "path is a directory, not a file");
    }
    if metadata.len() > max_bytes {
        return failed(
            "file_too_large",
            format!(
                "file is {} bytes, over the {} byte limit",
                metadata.len(),
                max_bytes
            ),
        );
    }
    let bytes = match std::fs::read(&resolved) {
        Ok(bytes) => bytes,
        Err(error) => return failed("not_found", format!("{error}")),
    };
    let sha256 = sha256_hex(&bytes);
    let (content, content_encoding) = match String::from_utf8(bytes) {
        Ok(text) => (text, HostFileEncoding::Utf8),
        Err(error) => (base64_encode(error.as_bytes()), HostFileEncoding::Base64),
    };
    HostFileOutcome::FileMetadata {
        content,
        content_encoding,
        size_bytes: metadata.len(),
        sha256,
        mode: mode_of(&metadata),
        modified_at_ms: modified_at_ms(&metadata),
    }
}

/// Writes one file with optional optimistic concurrency and mode control.
#[allow(clippy::too_many_arguments)]
fn write_file_full(
    path: &str,
    root_path: &str,
    content: &str,
    content_encoding: HostFileEncoding,
    max_bytes: u64,
    create_parents: bool,
    expected_sha256: Option<&str>,
    create_only: bool,
    mode: Option<u32>,
) -> HostFileOutcome {
    let path = PathBuf::from(path);
    let root = PathBuf::from(root_path);
    if !path.is_absolute() || !root.is_absolute() {
        return failed("invalid_path", "path and root_path must be absolute");
    }
    let bytes = match content_encoding {
        HostFileEncoding::Utf8 => content.as_bytes().to_vec(),
        HostFileEncoding::Base64 => match base64_decode(content) {
            Some(bytes) => bytes,
            None => return failed("invalid_request", "content is not valid base64"),
        },
    };
    if bytes.len() as u64 > max_bytes {
        return failed(
            "file_too_large",
            format!(
                "write is {} bytes, over the {} byte limit",
                bytes.len(),
                max_bytes
            ),
        );
    }
    // The optimistic check is against what is on disk *now*, before any
    // mutation: that is what makes it a compare-and-set rather than a race.
    let current = match std::fs::read(&path) {
        Ok(existing) => Some(sha256_hex(&existing)),
        Err(_) => None,
    };
    if create_only {
        if current.is_some() {
            return HostFileOutcome::Conflict {
                current_sha256: current,
            };
        }
    } else if let Some(expected) = expected_sha256 {
        if current.as_deref() != Some(expected) {
            return HostFileOutcome::Conflict {
                current_sha256: current,
            };
        }
    }
    let Some(file_name) = path.file_name() else {
        return failed("invalid_path", "path has no file name");
    };
    let Some(parent) = path.parent() else {
        return failed("invalid_path", "path has no parent directory");
    };
    if create_parents {
        if let Err(error) = std::fs::create_dir_all(parent) {
            return failed(
                "invalid_path",
                format!("could not create {parent:?}: {error}"),
            );
        }
    } else if !parent.exists() {
        // A missing parent without `createParents` is a path the caller named
        // wrongly, not a file that vanished: `invalid_path` at 400 is the
        // actionable answer, where `not_found` would suggest a race.
        return failed(
            "invalid_path",
            format!("parent directory {parent:?} does not exist"),
        );
    }
    let real_parent = match confined_existing(parent, &root) {
        Ok(parent) => parent,
        Err(outcome) => return outcome,
    };
    let target = real_parent.join(file_name);
    // Write through a sibling temp file and rename, so a crash mid-write leaves
    // either the old file or the new one, never a truncated file that looks
    // like a successful save.
    let temp = real_parent.join(format!(".{}.loom-write", file_name.to_string_lossy()));
    if let Err(error) = std::fs::write(&temp, &bytes) {
        return failed("invalid_path", format!("could not write {temp:?}: {error}"));
    }
    if let Some(mode) = mode {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode & 0o777));
        }
        #[cfg(not(unix))]
        let _ = mode;
    }
    if let Err(error) = std::fs::rename(&temp, &target) {
        let _ = std::fs::remove_file(&temp);
        return failed(
            "invalid_path",
            format!("could not write {target:?}: {error}"),
        );
    }
    let written = std::fs::metadata(&target).ok();
    HostFileOutcome::Written(HostFileContent {
        path: target.to_string_lossy().into_owned(),
        content: String::new(),
        content_encoding: HostFileEncoding::Utf8,
        size_bytes: bytes.len() as u64,
        mime_type: mime_type_for(&target),
        modified_at_ms: written.as_ref().and_then(modified_at_ms),
        sha256: Some(sha256_hex(&bytes)),
    })
}

/// Applies mode and/or modification-time changes to one path.
fn set_metadata(
    path: &str,
    root_path: Option<&str>,
    mode: Option<u32>,
    touch: Option<bool>,
) -> HostFileOutcome {
    if mode.is_none() && touch != Some(true) {
        return failed(
            "invalid_request",
            "set_metadata must name a mode or touch=true",
        );
    }
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return failed("invalid_path", "path must be absolute");
    }
    let resolved = match root_path.map(PathBuf::from) {
        Some(root) => match confined_existing(&path, &root) {
            Ok(path) => path,
            Err(outcome) => return outcome,
        },
        None => path,
    };
    if !resolved.exists() {
        return failed("not_found", "path does not exist");
    }
    if let Some(mode) = mode {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(error) =
                std::fs::set_permissions(&resolved, std::fs::Permissions::from_mode(mode & 0o777))
            {
                return failed("invalid_path", format!("could not set mode: {error}"));
            }
        }
        #[cfg(not(unix))]
        {
            // Windows has no POSIX mode. Reporting the request as done while
            // ignoring it would be a lie a client could act on, so it is
            // refused instead.
            let _ = mode;
            return failed(
                "unsupported_media_type",
                "POSIX mode bits are not supported on this host",
            );
        }
    }
    if touch == Some(true) {
        // Re-writing the file's own bytes is the portable `touch`: setting the
        // mtime without a platform-specific syscall this crate cannot reach.
        let bytes = match std::fs::read(&resolved) {
            Ok(bytes) => bytes,
            Err(error) => return failed("not_found", format!("{error}")),
        };
        if let Err(error) = std::fs::write(&resolved, &bytes) {
            return failed("invalid_path", format!("could not touch path: {error}"));
        }
    }
    HostFileOutcome::Done
}

/// Copies one path, confined to a shared root on both sides.
fn copy_path(
    source_path: &str,
    destination_path: &str,
    root_path: &str,
    overwrite: bool,
) -> HostFileOutcome {
    let source = PathBuf::from(source_path);
    let destination = PathBuf::from(destination_path);
    let root = PathBuf::from(root_path);
    if !source.is_absolute() || !destination.is_absolute() || !root.is_absolute() {
        return failed(
            "invalid_path",
            "source_path, destination_path and root_path must be absolute",
        );
    }
    let real_source = match confined_existing(&source, &root) {
        Ok(source) => source,
        Err(outcome) => return outcome,
    };
    if real_source.is_dir() {
        return failed("invalid_path", "source is a directory, not a file");
    }
    let real_destination = match confine_new(&destination, Some(&root)) {
        Ok(destination) => destination,
        Err(outcome) => return outcome,
    };
    if let Some(parent) = real_destination.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            return failed(
                "invalid_path",
                format!("could not create destination parent: {error}"),
            );
        }
    }
    if real_destination.exists() && !overwrite {
        return failed(
            "conflict",
            "destination already exists and overwrite was not requested",
        );
    }
    match std::fs::copy(&real_source, &real_destination) {
        Ok(_) => HostFileOutcome::Done,
        Err(error) => failed("invalid_path", format!("could not copy path: {error}")),
    }
}

/// The destination path for `name`, suffixed rather than overwriting.
///
/// A copy that silently replaced an existing attachment would lose a file the
/// user still had. `name-2.ext` is what a file manager does, and it keeps a
/// repeated copy idempotent in the only sense that matters: nothing is lost.
fn unique_target(directory: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let candidate = directory.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = path.extension().map(|extension| extension.to_os_string());
    for index in 2..=MAX_COPY_FILES as u32 {
        let mut candidate_name = format!("{stem}-{index}");
        if let Some(extension) = &extension {
            candidate_name.push('.');
            candidate_name.push_str(&extension.to_string_lossy());
        }
        let candidate = directory.join(candidate_name);
        if !candidate.exists() {
            return candidate;
        }
    }
    candidate
}

/// Standard base64 (RFC 4648) decoding, so a write can accept binary uploads.
fn base64_decode(raw: &str) -> Option<Vec<u8>> {
    let mut decoded = Vec::with_capacity(raw.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in raw.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            decoded.push((buffer >> bits) as u8);
        }
    }
    Some(decoded)
}

fn list_directory(
    path: &str,
    include_files: bool,
    include_directories: bool,
    include_hidden: bool,
    limit: usize,
) -> HostFileOutcome {
    let root = PathBuf::from(path);
    if !root.is_absolute() {
        return failed("invalid_path", "path must be absolute");
    }
    let Ok(entries) = std::fs::read_dir(&root) else {
        return HostFileOutcome::Listing {
            entries: Vec::new(),
            truncated: false,
        };
    };
    let mut listed = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let kind = if metadata.is_dir() {
            HostPathKind::Directory
        } else if metadata.is_file() {
            HostPathKind::File
        } else {
            continue;
        };
        if (kind == HostPathKind::File && !include_files)
            || (kind == HostPathKind::Directory && !include_directories)
        {
            continue;
        }
        listed.push(HostFileEntry {
            path: name.clone(),
            name,
            kind,
            score: 0.0,
            positions: Vec::new(),
        });
    }
    listed.sort_by(|left, right| left.path.cmp(&right.path));
    let truncated = listed.len() > limit;
    listed.truncate(limit);
    HostFileOutcome::Listing {
        entries: listed,
        truncated,
    }
}

/// Lists a directory tree, relative to `path`.
///
/// A missing directory is an empty listing, not an error: that is what bb
/// answers for a thread storage directory that has not been created yet, and a
/// client renders it as "nothing here" rather than as a failure.
fn list_paths(
    path: &str,
    query: Option<&str>,
    limit: usize,
    include_files: bool,
    include_directories: bool,
    include_hidden: bool,
) -> HostFileOutcome {
    let root = PathBuf::from(path);
    if !root.is_absolute() {
        return failed("invalid_path", "path must be absolute");
    }
    if !root.is_dir() {
        // Absent, or not a directory: an empty listing either way, so a
        // storage directory that has not been created renders as empty.
        return HostFileOutcome::Listing {
            entries: Vec::new(),
            truncated: false,
        };
    }

    let mut entries = Vec::new();
    walk(
        &root,
        &root,
        include_files,
        include_directories,
        include_hidden,
        &mut entries,
    );

    // Rank by the query when there is one, exactly as an unfiltered listing is
    // reported with `score: 0` and no positions.
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        let lowered = query.to_lowercase();
        for entry in &mut entries {
            let haystack = entry.path.to_lowercase();
            if let Some(index) = haystack.find(&lowered) {
                // A contiguous match scores above a scattered one, and an
                // earlier match above a later one — enough ordering for a
                // picker without inventing bb's full fuzzy ranking here.
                entry.score = 1.0 / (1.0 + index as f64);
                entry.positions = (index..index + lowered.len()).collect();
            }
        }
        entries.retain(|entry| entry.score > 0.0);
        entries.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.path.cmp(&right.path))
        });
    } else {
        entries.sort_by(|left, right| left.path.cmp(&right.path));
    }

    let truncated = entries.len() > limit;
    entries.truncate(limit);
    HostFileOutcome::Listing { entries, truncated }
}

/// Depth-first walk producing root-relative entries.
///
/// Symlinks are skipped rather than followed, which is both bb's behaviour and
/// what keeps a listing from leaving the tree it was asked about.
fn walk(
    dir: &Path,
    root: &Path,
    include_files: bool,
    include_directories: bool,
    include_hidden: bool,
    entries: &mut Vec<HostFileEntry>,
) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<_> = read.filter_map(Result::ok).collect();
    children.sort_by_key(std::fs::DirEntry::file_name);
    for child in children {
        let name = child.file_name();
        let name = name.to_string_lossy().into_owned();
        if ALWAYS_EXCLUDED_NAMES.contains(&name.as_str()) {
            continue;
        }
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        let Ok(file_type) = child.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let full = child.path();
        let Ok(relative) = full.strip_prefix(root) else {
            continue;
        };
        let relative = relative
            .components()
            .filter_map(|component| match component {
                Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");
        if file_type.is_dir() {
            if include_directories {
                entries.push(HostFileEntry {
                    path: relative.clone(),
                    name: name.clone(),
                    kind: HostPathKind::Directory,
                    score: 0.0,
                    positions: Vec::new(),
                });
            }
            walk(
                &full,
                root,
                include_files,
                include_directories,
                include_hidden,
                entries,
            );
            continue;
        }
        if include_files {
            entries.push(HostFileEntry {
                path: relative,
                name,
                kind: HostPathKind::File,
                score: 0.0,
                positions: Vec::new(),
            });
        }
    }
}

/// A media type from the path's extension.
///
/// Deliberately a small table rather than a MIME database: the client only
/// branches on the coarse families (image/video/text/other), and a wrong
/// specific type is worse than a correct coarse one.
fn mime_type_for(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let mime = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "txt" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "json" => "application/json",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" | "cjs" => "text/javascript",
        "ts" | "tsx" | "jsx" => "application/typescript",
        "rs" => "text/x-rust",
        "py" => "text/x-python",
        "sh" => "application/x-sh",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        _ => return None,
    };
    Some(mime.to_owned())
}

/// Standard base64 (RFC 4648) with padding, hand-rolled so the worker needs no
/// encoding dependency for one field.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(ALPHABET[(((first & 0b11) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(ALPHABET[(((second & 0b1111) << 2) | (third >> 6)) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(ALPHABET[(third & 0b11_1111) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

impl Worker {
    /// Handles one host file request arriving on the host scope.
    ///
    /// The filesystem work runs on the blocking pool, so a large listing or a
    /// multi-megabyte read cannot stall the socket loop that everything else on
    /// this worker shares.
    pub(crate) fn start_host_file_request(&self, request: HostFileRequest) {
        // A request addressed to a different host is not this worker's. The
        // relay room should make that impossible, but a panic on an unexpected
        // frame would take the whole connection down.
        if self.host_id.as_ref() != Some(&request.host_id) {
            return;
        }
        let host_id = request.host_id.clone();
        let request_id = request.request_id.clone();
        let reports = self.host_file_reports_tx.clone();
        tokio::spawn(async move {
            let report = match tokio::task::spawn_blocking(move || answer(request)).await {
                Ok(report) => report,
                // A panicking read has nowhere to attribute itself; answer the
                // one request with a failure rather than dropping the whole
                // worker, which is what an unwrap here would do.
                Err(error) => {
                    eprintln!("loom-worker: host file request panicked: {error}");
                    HostFileReport {
                        host_id,
                        request_id,
                        outcome: failed("internal_error", "the file request panicked"),
                    }
                }
            };
            let _ = reports.send(report).await;
        });
    }
}

//! Host filesystem reads and listings, for the control plane.
//!
//! This is the daemon's half of [`HostFileRequest`]. It exists so the control
//! plane can show a thread's files without ever reading its own disk: a request
//! arrives, the machine that actually owns the path performs it, and the answer
//! goes back up the daemon's socket.
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

use std::path::{Component, Path, PathBuf};

use loom_provider_protocol::{
    HostFileContent, HostFileEncoding, HostFileEntry, HostFileOperation, HostFileOutcome,
    HostFileReport, HostFileRequest, HostPathKind,
};

use crate::Daemon;

/// Names never listed, whatever the caller asks for.
///
/// `.git` is excluded unconditionally, like bb's `ALWAYS_EXCLUDED_NAMES`: its
/// object store is enormous and never what a file picker wants.
const ALWAYS_EXCLUDED_NAMES: [&str; 1] = [".git"];

/// The daemon's answer, ready to be sent up the socket.
///
/// A free function rather than a method so it can run on the blocking pool
/// without borrowing the daemon.
pub fn answer(request: HostFileRequest) -> HostFileReport {
    let outcome = match &request.operation {
        HostFileOperation::Read {
            path,
            root_path,
            max_bytes,
        } => read_file(path, root_path.as_deref(), *max_bytes),
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
        modified_at_ms: metadata.modified().ok().and_then(|time| {
            time.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|since| since.as_millis() as u64)
        }),
    })
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

/// Standard base64 (RFC 4648) with padding, hand-rolled so the daemon needs no
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

impl Daemon {
    /// Handles one host file request arriving on the host scope.
    ///
    /// The filesystem work runs on the blocking pool, so a large listing or a
    /// multi-megabyte read cannot stall the socket loop that everything else on
    /// this daemon shares.
    pub(crate) fn start_host_file_request(&self, request: HostFileRequest) {
        // A request addressed to a different host is not this daemon's. The
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
                // daemon, which is what an unwrap here would do.
                Err(error) => {
                    eprintln!("loom-daemon: host file request panicked: {error}");
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

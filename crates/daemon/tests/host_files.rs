//! The daemon's host file reads and listings, against a real filesystem.
//!
//! The server's B5 conformance tests use a scripted host, because what they
//! must prove is that the *control plane* asks a machine rather than reading its
//! own disk. This file proves the other half: the machine's answer is correct,
//! confined to the root it was given, and honest about size and encoding.

use std::path::{Path, PathBuf};

use loom_daemon::host_files::answer;
use loom_domain::HostId;
use loom_provider_protocol::{
    HostFileEncoding, HostFileOperation, HostFileOutcome, HostFileRequest, HostPathKind,
};

fn request(operation: HostFileOperation) -> HostFileRequest {
    HostFileRequest {
        request_id: "req-1".into(),
        host_id: HostId::mint(),
        operation,
        created_at_ms: 1,
    }
}

fn read(path: &Path, root: Option<&Path>, max_bytes: u64) -> HostFileOutcome {
    answer(request(HostFileOperation::Read {
        path: path.to_string_lossy().into_owned(),
        root_path: root.map(|root| root.to_string_lossy().into_owned()),
        max_bytes,
    }))
    .outcome
}

/// A listing through the real entry point, so the request shape is exercised.
fn list(path: &Path, query: Option<&str>, limit: usize) -> HostFileOutcome {
    list_kinds(path, query, limit, true, true, false)
}

#[allow(clippy::too_many_arguments)]
fn list_kinds(
    path: &Path,
    query: Option<&str>,
    limit: usize,
    include_files: bool,
    include_directories: bool,
    include_hidden: bool,
) -> HostFileOutcome {
    answer(request(HostFileOperation::List {
        path: path.to_string_lossy().into_owned(),
        query: query.map(str::to_owned),
        limit,
        include_files,
        include_directories,
        include_hidden,
    }))
    .outcome
}

fn content(outcome: HostFileOutcome) -> loom_provider_protocol::HostFileContent {
    match outcome {
        HostFileOutcome::Content(content) => content,
        other => panic!("expected content, got {other:?}"),
    }
}

fn failure(outcome: HostFileOutcome) -> (String, String) {
    match outcome {
        HostFileOutcome::Failed { code, message } => (code, message),
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn write(dir: &Path, relative: &str, contents: &[u8]) -> PathBuf {
    let path = dir.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, contents).unwrap();
    path
}

#[test]
fn a_utf8_file_is_read_verbatim_with_its_media_type_and_size() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "src/lib.rs", b"pub fn hi() {}\n");

    let read = content(read(&path, None, 1024));
    assert_eq!(read.content, "pub fn hi() {}\n");
    assert_eq!(read.content_encoding, HostFileEncoding::Utf8);
    assert_eq!(read.size_bytes, 15);
    assert_eq!(read.mime_type.as_deref(), Some("text/x-rust"));
    assert!(read.modified_at_ms.is_some());
    // The absolute path is echoed, so a caller cannot confuse it with the
    // root-relative one it asked for.
    assert_eq!(read.path, path.to_string_lossy());
}

#[test]
fn non_utf8_bytes_travel_as_base64_and_survive_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = (0u8..=255).collect();
    let path = write(dir.path(), "blob.bin", &bytes);

    let read = content(read(&path, None, 1024));
    assert_eq!(read.content_encoding, HostFileEncoding::Base64);
    assert_eq!(read.size_bytes, 256);
    assert_eq!(decode_base64(&read.content), bytes);
}

#[test]
fn a_file_over_the_limit_is_refused_rather_than_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "big.txt", &[b'x'; 100]);

    let (code, message) = failure(read(&path, None, 99));
    assert_eq!(code, "file_too_large");
    assert!(message.contains("100"), "{message}");

    // At the limit it is served, so the boundary is exact and not off by one.
    let at_limit = content(read(&path, None, 100));
    assert_eq!(at_limit.size_bytes, 100);
}

#[test]
fn a_missing_file_and_a_directory_are_each_refused_with_their_own_code() {
    let dir = tempfile::tempdir().unwrap();
    let (code, _) = failure(read(&dir.path().join("absent.txt"), None, 1024));
    assert_eq!(code, "not_found");

    let (code, message) = failure(read(dir.path(), None, 1024));
    assert_eq!(code, "invalid_path");
    assert!(message.contains("directory"), "{message}");

    // A relative path is refused outright: this daemon has no cwd to resolve it
    // against, and guessing one is how the wrong file gets served.
    let (code, _) = failure(read(Path::new("relative.txt"), None, 1024));
    assert_eq!(code, "invalid_path");
}

#[test]
fn a_read_root_confines_the_file_including_through_a_symlink() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    let inside = write(&root, "src/lib.rs", b"inside\n");
    let outside = write(temp.path(), "outside.txt", b"outside\n");

    // The contained file is served.
    assert_eq!(
        content(read(&inside, Some(&root), 1024)).content,
        "inside\n"
    );

    // A path that is outside the root is refused even though it exists.
    let (code, message) = failure(read(&outside, Some(&root), 1024));
    assert_eq!(code, "invalid_path");
    assert!(message.contains("escapes"), "{message}");

    // A symlink inside the root pointing outside it is the case the control
    // plane cannot see: the path looks contained until it is resolved.
    #[cfg(unix)]
    {
        let link = root.join("escape.txt");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let (code, message) = failure(read(&link, Some(&root), 1024));
        assert_eq!(code, "invalid_path");
        assert!(message.contains("escapes"), "{message}");
    }

    // A root that does not exist is a bad request, not a missing file.
    let (code, _) = failure(read(&inside, Some(&temp.path().join("absent")), 1024));
    assert_eq!(code, "invalid_path");
}

#[test]
fn a_listing_walks_the_tree_relative_to_its_root_and_skips_hidden_and_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "notes.md", b"# notes\n");
    write(dir.path(), "deep/report.csv", b"a,b\n");
    write(dir.path(), ".hidden", b"secret");
    std::fs::create_dir_all(dir.path().join(".git/objects")).unwrap();
    std::fs::write(dir.path().join(".git/objects/abc"), b"obj").unwrap();

    let outcome = list(dir.path(), None, 100);
    let HostFileOutcome::Listing { entries, truncated } = outcome else {
        panic!("expected a listing, got {outcome:?}");
    };
    assert!(!truncated);
    let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    // Sorted, relative, `/`-separated, and including directories when asked.
    assert_eq!(paths, vec!["deep", "deep/report.csv", "notes.md"]);
    assert_eq!(entries[0].kind, HostPathKind::Directory);
    assert_eq!(entries[1].kind, HostPathKind::File);
    assert_eq!(entries[1].name, "report.csv");
    // Without a query there is no score, exactly as the contract declares.
    assert!(entries.iter().all(|entry| entry.score == 0.0));
    assert!(entries.iter().all(|entry| entry.positions.is_empty()));

    // Files only is a different listing over the same tree.
    let files_only = list_kinds(dir.path(), None, 100, true, false, false);
    let HostFileOutcome::Listing { entries, .. } = files_only else {
        panic!("expected a listing");
    };
    assert!(entries.iter().all(|entry| entry.kind == HostPathKind::File));

    // Dotfiles appear only when they are asked for, and `.git` never does.
    let hidden = list_kinds(dir.path(), None, 100, true, true, true);
    let HostFileOutcome::Listing { entries, .. } = hidden else {
        panic!("expected a listing");
    };
    let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    assert!(paths.contains(&".hidden"), "{paths:?}");
    assert!(
        !paths.iter().any(|path| path.starts_with(".git")),
        "the git object store is never listed: {paths:?}"
    );

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(dir.path().join("notes.md"), dir.path().join("link.md"))
            .unwrap();
        let HostFileOutcome::Listing { entries, .. } = list(dir.path(), None, 100) else {
            panic!("expected a listing");
        };
        assert!(!entries.iter().any(|entry| entry.path == "link.md"));
    }
}

#[test]
fn a_listing_of_an_absent_directory_is_empty_rather_than_an_error() {
    let dir = tempfile::tempdir().unwrap();
    // A thread's storage directory may not exist yet; a client renders that as
    // "nothing here", not as a failure.
    let outcome = list(&dir.path().join("never-created"), None, 100);
    let HostFileOutcome::Listing { entries, truncated } = outcome else {
        panic!("expected a listing, got {outcome:?}");
    };
    assert!(entries.is_empty());
    assert!(!truncated);

    // A file is not a directory either, and answers the same empty listing.
    let file = write(dir.path(), "a.txt", b"x");
    let HostFileOutcome::Listing { entries, .. } = list(&file, None, 100) else {
        panic!("expected a listing");
    };
    assert!(entries.is_empty());
}

#[test]
fn a_query_filters_and_ranks_and_a_limit_truncates_loudly() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "notes.md", b"1");
    write(dir.path(), "src/no-notes.txt", b"2");
    write(dir.path(), "other.txt", b"3");

    let outcome = list(dir.path(), Some("notes"), 100);
    let HostFileOutcome::Listing { entries, truncated } = outcome else {
        panic!("expected a listing");
    };
    assert!(!truncated);
    assert_eq!(entries.len(), 2);
    // Both matches carry the offsets of the match, which is what a client
    // highlights.
    for entry in &entries {
        assert!(entry.score > 0.0, "{entry:?}");
        assert_eq!(entry.positions.len(), "notes".len());
    }
    // An earlier match ranks above a later one.
    assert_eq!(entries[0].path, "notes.md");

    let no_match = list(dir.path(), Some("zzz"), 100);
    let HostFileOutcome::Listing { entries, .. } = no_match else {
        panic!("expected a listing");
    };
    assert!(entries.is_empty());

    // A limit below the match count reports truncation, never a silent cut.
    let truncated_outcome = list(dir.path(), None, 2);
    let HostFileOutcome::Listing { entries, truncated } = truncated_outcome else {
        panic!("expected a listing");
    };
    assert_eq!(entries.len(), 2);
    assert!(truncated);
}

#[test]
fn the_report_carries_the_request_identity_it_answers() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "a.txt", b"x");
    let request = HostFileRequest {
        request_id: "correlation-9".into(),
        host_id: HostId::mint(),
        operation: HostFileOperation::Read {
            path: path.to_string_lossy().into_owned(),
            root_path: None,
            max_bytes: 1024,
        },
        created_at_ms: 5,
    };
    let host_id = request.host_id.clone();
    let report = answer(request);
    // Correlation is what a server waits on; echoing it back is the contract
    // between the two halves.
    assert_eq!(report.request_id, "correlation-9");
    assert_eq!(report.host_id, host_id);
}

/* ------------------------------------------------------------------ */
/* Helpers                                                             */
/* ------------------------------------------------------------------ */

/// A local decoder, so this test does not reach into the server's private one.
fn decode_base64(raw: &str) -> Vec<u8> {
    let mut decoded = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in raw.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => continue,
            other => panic!("unexpected base64 byte {other:?}"),
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            decoded.push((buffer >> bits) as u8);
        }
    }
    decoded
}

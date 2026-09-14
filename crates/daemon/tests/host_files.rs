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
/* Writes and copies (B7 attachments)                                  */
/* ------------------------------------------------------------------ */

/// Encodes bytes the way the control plane does, so a write can be exercised
/// end to end without reaching into either crate's private encoder.
fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::new();
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(ALPHABET[(((first & 0b11) << 4) | (second >> 4)) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[(((second & 0b1111) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(third & 0b11_1111) as usize] as char
        } else {
            '='
        });
    }
    encoded
}

fn write_file(
    path: &Path,
    root: &Path,
    contents: &[u8],
    max_bytes: u64,
    overwrite: bool,
) -> HostFileOutcome {
    answer(request(HostFileOperation::Write {
        path: path.to_string_lossy().into_owned(),
        root_path: root.to_string_lossy().into_owned(),
        content: encode_base64(contents),
        max_bytes,
        overwrite,
    }))
    .outcome
}

#[test]
fn a_write_creates_parent_directories_and_decodes_the_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("nested/deeper/notes.txt");
    let outcome = write_file(&target, dir.path(), b"hello", 1024, false);
    let HostFileOutcome::Written(written) = outcome else {
        panic!("expected a write, got {outcome:?}");
    };
    assert_eq!(written.size_bytes, 5);
    assert_eq!(std::fs::read(&target).unwrap(), b"hello");
    assert_eq!(written.mime_type.as_deref(), Some("text/plain"));
    // The response names the real path, which is what a client must send back.
    assert!(written.path.ends_with("nested/deeper/notes.txt"));
}

#[test]
fn a_write_suffixes_a_collision_instead_of_clobbering() {
    let dir = tempfile::tempdir().unwrap();
    let target = write(dir.path(), "a.txt", b"original");
    let outcome = write_file(&target, dir.path(), b"replacement", 1024, false);
    let HostFileOutcome::Written(written) = outcome else {
        panic!("expected a write, got {outcome:?}");
    };
    // The original survives; the new bytes land at a suffixed sibling.
    assert_eq!(std::fs::read(&target).unwrap(), b"original");
    assert!(written.path.ends_with("a-2.txt"));
    assert_eq!(
        std::fs::read(dir.path().join("a-2.txt")).unwrap(),
        b"replacement"
    );

    // With `overwrite`, the same path is replaced in place.
    let overwritten = write_file(&target, dir.path(), b"replacement", 1024, true);
    let HostFileOutcome::Written(written) = overwritten else {
        panic!("expected a write, got {overwritten:?}");
    };
    assert_eq!(written.path, target.to_string_lossy());
    assert_eq!(std::fs::read(&target).unwrap(), b"replacement");
}

#[test]
fn a_write_outside_the_root_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    // The target's parent is outside the root, and the parent is what is
    // resolved: the target does not exist yet.
    let escape = dir.path().join("outside/escaped.txt");
    let outcome = write_file(&escape, &root, b"x", 1024, false);
    let HostFileOutcome::Failed { code, .. } = outcome else {
        panic!("expected a refusal, got {outcome:?}");
    };
    assert_eq!(code, "invalid_path");
    assert!(!escape.exists());
}

#[test]
fn a_write_larger_than_the_limit_is_refused_not_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("big.bin");
    let outcome = write_file(&target, dir.path(), &[7u8; 2048], 1024, false);
    let HostFileOutcome::Failed { code, .. } = outcome else {
        panic!("expected a refusal, got {outcome:?}");
    };
    assert_eq!(code, "file_too_large");
    assert!(
        !target.exists(),
        "a refused write must leave nothing behind"
    );
}

#[test]
fn a_write_of_malformed_base64_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = answer(request(HostFileOperation::Write {
        path: dir.path().join("x.bin").to_string_lossy().into_owned(),
        root_path: dir.path().to_string_lossy().into_owned(),
        content: "not base64!".into(),
        max_bytes: 1024,
        overwrite: false,
    }))
    .outcome;
    let HostFileOutcome::Failed { code, .. } = outcome else {
        panic!("expected a refusal, got {outcome:?}");
    };
    assert_eq!(code, "invalid_request");
}

fn copy(
    paths: &[&Path],
    source_root: &Path,
    destination: &Path,
    destination_root: &Path,
) -> HostFileOutcome {
    answer(request(HostFileOperation::Copy {
        paths: paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        source_root: source_root.to_string_lossy().into_owned(),
        destination: destination.to_string_lossy().into_owned(),
        destination_root: destination_root.to_string_lossy().into_owned(),
        max_bytes: 1024,
    }))
    .outcome
}

#[test]
fn a_copy_takes_each_path_and_reports_only_the_failures() {
    let dir = tempfile::tempdir().unwrap();
    let source_root = dir.path().join("source");
    let destination_root = dir.path().join("destination");
    std::fs::create_dir_all(&source_root).unwrap();
    std::fs::create_dir_all(&destination_root).unwrap();
    let present = write(&source_root, "a.txt", b"a");
    let missing = source_root.join("gone.txt");

    let outcome = copy(
        &[&present, &missing],
        &source_root,
        &destination_root,
        &destination_root,
    );
    let HostFileOutcome::Copied { files, failures } = outcome else {
        panic!("expected a copy, got {outcome:?}");
    };
    assert_eq!(files.len(), 1);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].code, "not_found");
    assert!(failures[0].path.ends_with("gone.txt"));
    assert_eq!(std::fs::read(destination_root.join("a.txt")).unwrap(), b"a");
}

#[test]
fn a_copy_never_loses_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let source_root = dir.path().join("source");
    let destination_root = dir.path().join("destination");
    std::fs::create_dir_all(&source_root).unwrap();
    std::fs::create_dir_all(&destination_root).unwrap();
    let source = write(&source_root, "a.txt", b"incoming");
    write(&destination_root, "a.txt", b"already here");

    let outcome = copy(
        &[&source],
        &source_root,
        &destination_root,
        &destination_root,
    );
    let HostFileOutcome::Copied { files, .. } = outcome else {
        panic!("expected a copy, got {outcome:?}");
    };
    // The collision is suffixed rather than overwritten.
    assert_eq!(
        std::fs::read(destination_root.join("a.txt")).unwrap(),
        b"already here"
    );
    assert!(files[0].path.ends_with("a-2.txt"));
    assert_eq!(
        std::fs::read(destination_root.join("a-2.txt")).unwrap(),
        b"incoming"
    );
}

#[test]
fn a_copy_out_of_the_source_root_is_reported_per_path() {
    let dir = tempfile::tempdir().unwrap();
    let source_root = dir.path().join("source");
    let destination_root = dir.path().join("destination");
    std::fs::create_dir_all(&source_root).unwrap();
    std::fs::create_dir_all(&destination_root).unwrap();
    let outside = write(dir.path(), "secret.txt", b"secret");

    let outcome = copy(
        &[&outside],
        &source_root,
        &destination_root,
        &destination_root,
    );
    let HostFileOutcome::Copied { files, failures } = outcome else {
        panic!("expected a copy, got {outcome:?}");
    };
    assert!(files.is_empty());
    assert_eq!(failures[0].code, "invalid_path");
}

#[test]
fn a_copy_to_a_destination_outside_its_root_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let source_root = dir.path().join("source");
    std::fs::create_dir_all(&source_root).unwrap();
    let source = write(&source_root, "a.txt", b"a");
    let destination = dir.path().join("somewhere-else");
    let destination_root = dir.path().join("declared-root");
    std::fs::create_dir_all(&destination_root).unwrap();

    let outcome = copy(&[&source], &source_root, &destination, &destination_root);
    let HostFileOutcome::Failed { code, .. } = outcome else {
        panic!("expected a refusal, got {outcome:?}");
    };
    assert_eq!(code, "invalid_path");
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

/* ------------------------------------------------------------------ */
/* B9 file operations                                                  */
/* ------------------------------------------------------------------ */

fn path_operation(operation: HostFileOperation) -> HostFileOutcome {
    answer(request(operation)).outcome
}

fn metadata_read(path: &Path, root: Option<&Path>, max_bytes: u64) -> HostFileOutcome {
    path_operation(HostFileOperation::ReadWithMetadata {
        path: path.to_string_lossy().into_owned(),
        root_path: root.map(|root| root.to_string_lossy().into_owned()),
        max_bytes,
    })
}

#[allow(clippy::too_many_arguments)]
fn full_write(
    path: &Path,
    root: &Path,
    content: &str,
    encoding: HostFileEncoding,
    create_parents: bool,
    expected_sha256: Option<&str>,
    create_only: bool,
    mode: Option<u32>,
) -> HostFileOutcome {
    path_operation(HostFileOperation::WriteFile {
        path: path.to_string_lossy().into_owned(),
        root_path: root.to_string_lossy().into_owned(),
        content: content.to_owned(),
        content_encoding: encoding,
        max_bytes: 1 << 20,
        create_parents,
        expected_sha256: expected_sha256.map(str::to_owned),
        create_only,
        mode,
    })
}

#[test]
fn a_directory_is_created_and_an_existing_one_is_success() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("a/b/c");

    let created = path_operation(HostFileOperation::CreateDirectory {
        path: target.to_string_lossy().into_owned(),
        root_path: Some(dir.path().to_string_lossy().into_owned()),
        recursive: true,
    });
    assert_eq!(created, HostFileOutcome::Done);
    assert!(target.is_dir());

    // Mkdir on an existing directory is idempotent: reporting a conflict would
    // be a race the caller cannot act on.
    let again = path_operation(HostFileOperation::CreateDirectory {
        path: target.to_string_lossy().into_owned(),
        root_path: Some(dir.path().to_string_lossy().into_owned()),
        recursive: true,
    });
    assert_eq!(again, HostFileOutcome::Done);
}

#[test]
fn a_non_recursive_mkdir_refuses_a_missing_parent() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("a/b");
    let outcome = path_operation(HostFileOperation::CreateDirectory {
        path: target.to_string_lossy().into_owned(),
        root_path: Some(dir.path().to_string_lossy().into_owned()),
        recursive: false,
    });
    let HostFileOutcome::Failed { code, .. } = outcome else {
        panic!("expected a refusal, got {outcome:?}");
    };
    assert_eq!(code, "invalid_path");
}

#[test]
fn a_mkdir_that_escapes_its_root_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let outside = dir.path().join("outside");
    let outcome = path_operation(HostFileOperation::CreateDirectory {
        path: outside.to_string_lossy().into_owned(),
        root_path: Some(root.to_string_lossy().into_owned()),
        recursive: true,
    });
    let HostFileOutcome::Failed { code, .. } = outcome else {
        panic!("expected a refusal, got {outcome:?}");
    };
    assert_eq!(code, "invalid_path");
    assert!(
        !outside.exists(),
        "the directory must not have been created"
    );
}

#[test]
fn a_move_renames_and_refuses_to_clobber() {
    let dir = tempfile::tempdir().unwrap();
    let source = write(dir.path(), "a.txt", b"a");
    let existing = write(dir.path(), "b.txt", b"b");

    let refused = path_operation(HostFileOperation::Move {
        source_path: source.to_string_lossy().into_owned(),
        destination_path: existing.to_string_lossy().into_owned(),
        root_path: Some(dir.path().to_string_lossy().into_owned()),
        overwrite: false,
    });
    let HostFileOutcome::Failed { code, .. } = refused else {
        panic!("expected a refusal, got {refused:?}");
    };
    assert_eq!(code, "conflict");
    assert_eq!(std::fs::read(&existing).unwrap(), b"b");

    let moved = path_operation(HostFileOperation::Move {
        source_path: source.to_string_lossy().into_owned(),
        destination_path: dir.path().join("c.txt").to_string_lossy().into_owned(),
        root_path: Some(dir.path().to_string_lossy().into_owned()),
        overwrite: false,
    });
    assert_eq!(moved, HostFileOutcome::Done);
    assert!(!source.exists());
    assert_eq!(std::fs::read(dir.path().join("c.txt")).unwrap(), b"a");
}

#[test]
fn a_remove_takes_a_file_and_a_recursive_directory() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(dir.path(), "a.txt", b"a");
    let removed_file = path_operation(HostFileOperation::Remove {
        path: file.to_string_lossy().into_owned(),
        root_path: Some(dir.path().to_string_lossy().into_owned()),
        recursive: false,
    });
    assert_eq!(removed_file, HostFileOutcome::Done);
    assert!(!file.exists());

    write(dir.path(), "tree/inner/a.txt", b"a");
    let tree = dir.path().join("tree");
    // A non-empty directory without `recursive` is a conflict, and nothing is
    // removed.
    let refused = path_operation(HostFileOperation::Remove {
        path: tree.to_string_lossy().into_owned(),
        root_path: Some(dir.path().to_string_lossy().into_owned()),
        recursive: false,
    });
    let HostFileOutcome::Failed { code, .. } = refused else {
        panic!("expected a conflict, got {refused:?}");
    };
    assert_eq!(code, "conflict");
    assert!(tree.join("inner/a.txt").exists());

    let recursive = path_operation(HostFileOperation::Remove {
        path: tree.to_string_lossy().into_owned(),
        root_path: Some(dir.path().to_string_lossy().into_owned()),
        recursive: true,
    });
    assert_eq!(recursive, HostFileOutcome::Done);
    assert!(!tree.exists());
}

#[test]
fn a_missing_remove_is_reported_as_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = path_operation(HostFileOperation::Remove {
        path: dir.path().join("gone").to_string_lossy().into_owned(),
        root_path: Some(dir.path().to_string_lossy().into_owned()),
        recursive: false,
    });
    let HostFileOutcome::Failed { code, .. } = outcome else {
        panic!("expected not_found, got {outcome:?}");
    };
    assert_eq!(code, "not_found");
}

#[test]
fn a_metadata_read_reports_the_sha256_of_the_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "a.txt", b"abc");
    let outcome = metadata_read(&path, Some(dir.path()), 1024);
    let HostFileOutcome::FileMetadata {
        content,
        sha256,
        size_bytes,
        mode,
        ..
    } = outcome
    else {
        panic!("expected metadata, got {outcome:?}");
    };
    assert_eq!(content, "abc");
    assert_eq!(size_bytes, 3);
    // The known SHA-256 of "abc".
    assert_eq!(
        sha256,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    #[cfg(unix)]
    assert!(mode.is_some());
}

#[test]
fn a_directory_is_refused_by_a_metadata_read() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = metadata_read(dir.path(), Some(dir.path()), 1024);
    let HostFileOutcome::Failed { code, .. } = outcome else {
        panic!("expected a refusal, got {outcome:?}");
    };
    assert_eq!(code, "invalid_path");
}

#[test]
fn an_optimistic_write_is_refused_when_the_hash_does_not_match() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "a.txt", b"abc");

    let outcome = full_write(
        &path,
        dir.path(),
        "new",
        HostFileEncoding::Utf8,
        false,
        Some("0000"),
        false,
        None,
    );
    let HostFileOutcome::Conflict { current_sha256 } = outcome else {
        panic!("expected a conflict, got {outcome:?}");
    };
    assert_eq!(
        current_sha256.as_deref(),
        Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"abc");

    // The matching hash goes through and reports the hash of what was written.
    let written = full_write(
        &path,
        dir.path(),
        "new",
        HostFileEncoding::Utf8,
        false,
        Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        false,
        None,
    );
    let HostFileOutcome::Written(written) = written else {
        panic!("expected a write, got {written:?}");
    };
    assert_eq!(std::fs::read(&path).unwrap(), b"new");
    assert!(written.sha256.is_some());
}

#[test]
fn a_create_only_write_refuses_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "a.txt", b"old");
    let outcome = full_write(
        &path,
        dir.path(),
        "new",
        HostFileEncoding::Utf8,
        false,
        None,
        true,
        None,
    );
    let HostFileOutcome::Conflict { current_sha256 } = outcome else {
        panic!("expected a conflict, got {outcome:?}");
    };
    assert!(current_sha256.is_some());
    assert_eq!(std::fs::read(&path).unwrap(), b"old");
}

#[test]
fn a_create_only_write_creates_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.txt");
    let outcome = full_write(
        &path,
        dir.path(),
        "fresh",
        HostFileEncoding::Utf8,
        false,
        None,
        true,
        None,
    );
    let HostFileOutcome::Written(_) = outcome else {
        panic!("expected a write, got {outcome:?}");
    };
    assert_eq!(std::fs::read(&path).unwrap(), b"fresh");
}

#[test]
fn a_write_creating_parents_works_and_one_that_does_not_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("a/b/c.txt");
    let refused = full_write(
        &nested,
        dir.path(),
        "x",
        HostFileEncoding::Utf8,
        false,
        None,
        false,
        None,
    );
    let HostFileOutcome::Failed { code, .. } = refused else {
        panic!("expected a refusal, got {refused:?}");
    };
    assert_eq!(code, "invalid_path");

    let created = full_write(
        &nested,
        dir.path(),
        "x",
        HostFileEncoding::Utf8,
        true,
        None,
        false,
        None,
    );
    let HostFileOutcome::Written(_) = created else {
        panic!("expected a write, got {created:?}");
    };
    assert_eq!(std::fs::read(&nested).unwrap(), b"x");
}

#[test]
fn a_write_can_set_the_mode_bits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("script.sh");
    let outcome = full_write(
        &path,
        dir.path(),
        "#!/bin/sh\n",
        HostFileEncoding::Utf8,
        false,
        None,
        false,
        Some(0o755),
    );
    let HostFileOutcome::Written(_) = outcome else {
        panic!("expected a write, got {outcome:?}");
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }
}

#[test]
fn a_base64_write_decodes_the_bytes_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob.bin");
    let bytes: Vec<u8> = (0u8..=255).collect();
    let outcome = full_write(
        &path,
        dir.path(),
        &encode_base64(&bytes),
        HostFileEncoding::Base64,
        false,
        None,
        false,
        None,
    );
    let HostFileOutcome::Written(_) = outcome else {
        panic!("expected a write, got {outcome:?}");
    };
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn a_set_metadata_with_neither_mode_nor_touch_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "a.txt", b"a");
    let outcome = path_operation(HostFileOperation::SetMetadata {
        path: path.to_string_lossy().into_owned(),
        root_path: Some(dir.path().to_string_lossy().into_owned()),
        mode: None,
        touch: None,
    });
    let HostFileOutcome::Failed { code, .. } = outcome else {
        panic!("expected a refusal, got {outcome:?}");
    };
    assert_eq!(code, "invalid_request");
}

#[test]
fn a_copy_path_is_confined_to_its_shared_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let source = write(&root, "a.txt", b"a");

    let copied = path_operation(HostFileOperation::CopyPath {
        source_path: source.to_string_lossy().into_owned(),
        destination_path: root.join("b.txt").to_string_lossy().into_owned(),
        root_path: root.to_string_lossy().into_owned(),
        overwrite: false,
    });
    assert_eq!(copied, HostFileOutcome::Done);
    assert_eq!(std::fs::read(root.join("b.txt")).unwrap(), b"a");

    // A destination outside the root is refused.
    let outside = dir.path().join("outside.txt");
    let refused = path_operation(HostFileOperation::CopyPath {
        source_path: source.to_string_lossy().into_owned(),
        destination_path: outside.to_string_lossy().into_owned(),
        root_path: root.to_string_lossy().into_owned(),
        overwrite: false,
    });
    let HostFileOutcome::Failed { code, .. } = refused else {
        panic!("expected a refusal, got {refused:?}");
    };
    assert_eq!(code, "invalid_path");
    assert!(!outside.exists());
}

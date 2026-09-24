//! Managed git worktrees: the worker's half of a `git-worktree` environment.
//!
//! The control plane decides *what* to provision — the source checkout, the
//! branch, the base. This module owns *how*:
//! `<workspace_root>/<environment_id>` is the worktree and
//! `<...>.loom-completed`, next to it, records that include-copying finished.
//! Mutations against one repository are serialized in-process; `git worktree
//! add` and `remove` rewrite the source repository's worktree metadata and two
//! concurrent runs corrupt each other's. A cross-process lock is future work,
//! as is remote-fetching a base branch; see `docs/worktrees.md`.

use std::path::{Path, PathBuf};

use loom_domain::EnvironmentId;
use loom_provider_protocol::EnvironmentProvisionOutcome;

use crate::workspace::{run_git, Failure};

/// The file a checkout may add to copy ignored files into a new worktree.
const INCLUDE_FILE: &str = ".worktreeinclude";
/// Written next to the worktree once include copying has finished. Its first
/// line is the branch name and its second the base, so a retry can tell
/// "created but not finished" from "done".
const COMPLETED_SUFFIX: &str = ".loom-completed";
/// Bound on git plumbing output; `ls-files` over a large ignored tree is the
/// widest and still fits comfortably.
const GIT_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

/// Serializes worktree mutations inside one worker process.
static WORKTREE_OPS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The state a completion marker holds.
#[derive(Clone, Debug)]
struct Marker {
    branch: String,
    base: Option<String>,
}

/// What a finished worktree looks like, in the shape the report needs.
struct WorktreeDetails {
    path: PathBuf,
    branch_name: String,
    base_branch: String,
    default_branch: Option<String>,
}

/// Provisions one managed worktree, mapping failures to the report outcome.
pub async fn provision(
    root: &Path,
    environment_id: &EnvironmentId,
    source_path: &str,
    branch_name: &str,
    base_branch: Option<&str>,
) -> EnvironmentProvisionOutcome {
    match create(root, environment_id, source_path, branch_name, base_branch).await {
        Ok(details) => EnvironmentProvisionOutcome::Provisioned {
            path: details.path.to_string_lossy().into_owned(),
            branch_name: Some(details.branch_name),
            base_branch: Some(details.base_branch),
            default_branch: details.default_branch,
            is_git_repo: Some(true),
        },
        Err(error) => EnvironmentProvisionOutcome::Failed {
            error: error.message,
        },
    }
}

/// Removes a worktree loom owns (or a managed directory) and its marker.
///
/// The path must be one the worker provisioned — this is called with the
/// environment's recorded path after the control plane decided to destroy it.
/// An unregistered directory is removed as a plain directory; a missing path
/// is success, because teardown is idempotent under redelivery.
pub async fn remove(path: &Path) -> Result<(), String> {
    remove_inner(path).await.map_err(|error| error.message)
}

async fn remove_inner(path: &Path) -> Result<(), Failure> {
    let _guard = WORKTREE_OPS.lock().await;
    let marker = marker_path(path);
    let _ = tokio::fs::remove_file(&marker).await;
    match tokio::fs::symlink_metadata(path).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(Failure::new(
                "unknown",
                format!("could not inspect {}: {error}", path.display()),
            ))
        }
    }

    if is_git_repo(path).await {
        // Resolve the common directory and remove through git so the source
        // repository's worktree metadata does not go stale. A failure here is
        // reported rather than papered over with `rm -rf`, because that would
        // orphan the metadata.
        let common = run_git(
            path,
            vec!["rev-parse".into(), "--git-common-dir".into()],
            GIT_OUTPUT_BYTES,
        )
        .await
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .map_err(|error| {
            Failure::new(
                "worktree_remove_failed",
                format!(
                    "could not resolve the git directory of {}: {}",
                    path.display(),
                    error.message
                ),
            )
        })?;
        let common_path = if Path::new(&common).is_absolute() {
            PathBuf::from(&common)
        } else {
            path.join(&common)
        };
        run_git(
            path,
            vec![
                format!("--git-dir={}", common_path.display()),
                "worktree".into(),
                "remove".into(),
                "--force".into(),
                path.to_string_lossy().into_owned(),
            ],
            GIT_OUTPUT_BYTES,
        )
        .await
        .map_err(|error| {
            Failure::new(
                "worktree_remove_failed",
                format!("could not remove {}: {}", path.display(), error.message),
            )
        })?;
    }

    // `git worktree remove` already deleted the directory; a plain managed
    // directory is still there. NotFound is the normal case for the former.
    if let Err(error) = tokio::fs::remove_dir_all(path).await {
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(Failure::new(
                "remove_failed",
                format!("could not remove {}: {error}", path.display()),
            ));
        }
    }
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::remove_dir(parent).await;
    }
    Ok(())
}

async fn create(
    root: &Path,
    environment_id: &EnvironmentId,
    source_path: &str,
    branch_name: &str,
    base_branch: Option<&str>,
) -> Result<WorktreeDetails, Failure> {
    let _guard = WORKTREE_OPS.lock().await;
    let source = Path::new(source_path);
    let target = root.join(environment_id.to_string());
    let marker_path = marker_path(&target);

    if !is_directory(source).await {
        return Err(Failure::new(
            "not_found",
            format!(
                "workspace source {} is not a directory on this machine",
                source.display()
            ),
        ));
    }
    if !is_git_repo(source).await {
        return Err(Failure::new(
            "not_git_repo",
            format!(
                "workspace source {} is not a git checkout",
                source.display()
            ),
        ));
    }
    if has_commit(source).await.is_none() {
        return Err(Failure::new(
            "unborn_head",
            format!("workspace source {} has no commits", source.display()),
        ));
    }

    // A previous attempt, or a redelivery, may have created the worktree. It is
    // ours because it lives under the workspace root: adopt it when it is on
    // the expected branch, finish the include copy when it is not marked done,
    // and refuse anything else rather than deleting data we do not recognise.
    let existing_marker = read_marker(&marker_path).await;
    let default_branch = resolve_default_branch(source).await;
    let base = match existing_marker
        .as_ref()
        .and_then(|marker| marker.base.clone())
    {
        Some(base) => base,
        None => resolve_base(source, base_branch, default_branch.as_deref()).await?,
    };
    if tokio::fs::symlink_metadata(&target).await.is_ok() {
        if !is_on_branch(&target, branch_name).await {
            return Err(Failure::new(
                "path_in_use",
                format!(
                    "{} already exists and is not a worktree on branch {branch_name}",
                    target.display()
                ),
            ));
        }
        if existing_marker
            .as_ref()
            .is_none_or(|marker| marker.branch != branch_name)
        {
            copy_includes(source, &target).await?;
            write_marker(&marker_path, branch_name, &base).await?;
        }
        return Ok(worktree_details(target, branch_name, base, default_branch));
    }

    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|error| {
            Failure::new(
                "permission_denied",
                format!("could not create {}: {error}", parent.display()),
            )
        })?;
    }
    run_git(
        source,
        vec![
            "worktree".into(),
            "add".into(),
            "-B".into(),
            branch_name.into(),
            target.to_string_lossy().into_owned(),
            base.clone(),
        ],
        GIT_OUTPUT_BYTES,
    )
    .await
    .map_err(|error| {
        Failure::new(
            "worktree_add_failed",
            format!(
                "could not create a worktree at {} from {}: {}",
                target.display(),
                source.display(),
                error.message
            ),
        )
    })?;
    copy_includes(source, &target).await?;
    write_marker(&marker_path, branch_name, &base).await?;
    Ok(worktree_details(target, branch_name, base, default_branch))
}

fn worktree_details(
    path: PathBuf,
    branch_name: &str,
    base_branch: String,
    default_branch: Option<String>,
) -> WorktreeDetails {
    WorktreeDetails {
        path,
        branch_name: branch_name.to_owned(),
        base_branch,
        default_branch,
    }
}

/// Copies the files `.worktreeinclude` selects from the source into `target`.
///
/// Same semantics as bb: only ignored/untracked files listed by
/// `git ls-files --others --ignored --exclude-from=<source>/.worktreeinclude`
/// are copied; symlinks, already-present destinations and paths escaping the
/// worktree are skipped; per-file errors are skipped rather than fatal, because
/// one unreadable ignored file should not fail a whole environment.
async fn copy_includes(source: &Path, target: &Path) -> Result<(), Failure> {
    let include = source.join(INCLUDE_FILE);
    let contents = match tokio::fs::read_to_string(&include).await {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(Failure::new(
                "unknown",
                format!("could not read {}: {error}", include.display()),
            ))
        }
    };
    let has_pattern = contents.lines().any(|line| {
        let line = line.trim();
        !line.is_empty() && !line.starts_with('#')
    });
    if !has_pattern {
        return Ok(());
    }

    let output = run_git(
        source,
        vec![
            "ls-files".into(),
            "--others".into(),
            "--ignored".into(),
            format!("--exclude-from={}", include.display()),
            "-z".into(),
        ],
        GIT_OUTPUT_BYTES,
    )
    .await
    .map_err(|error| {
        Failure::new(
            "unknown",
            format!("could not list {}: {}", include.display(), error.message),
        )
    })?;
    let Ok(target_real) = tokio::fs::canonicalize(target).await else {
        return Err(Failure::new(
            "not_found",
            format!("worktree {} disappeared", target.display()),
        ));
    };

    let listing = String::from_utf8_lossy(&output.stdout);
    let mut skipped = 0usize;
    for relative in listing.split('\0').filter(|entry| !entry.is_empty()) {
        let from = source.join(relative);
        match tokio::fs::symlink_metadata(&from).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                skipped += 1;
                continue;
            }
            Ok(_) => {}
            Err(_) => {
                skipped += 1;
                continue;
            }
        }
        let to = target_real.join(relative);
        if tokio::fs::symlink_metadata(&to).await.is_ok() {
            skipped += 1;
            continue;
        }
        let Some(parent) = to.parent() else {
            skipped += 1;
            continue;
        };
        if tokio::fs::create_dir_all(parent).await.is_err() {
            skipped += 1;
            continue;
        }
        let Ok(parent_real) = tokio::fs::canonicalize(parent).await else {
            skipped += 1;
            continue;
        };
        if !parent_real.starts_with(&target_real) {
            skipped += 1;
            continue;
        }
        let Ok(mut from_file) = tokio::fs::File::open(&from).await else {
            skipped += 1;
            continue;
        };
        let Ok(mut to_file) = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&to)
            .await
        else {
            skipped += 1;
            continue;
        };
        if tokio::io::copy(&mut from_file, &mut to_file).await.is_err() {
            skipped += 1;
        }
    }
    if skipped > 0 {
        eprintln!(
            "loom-worker: skipped {skipped} .worktreeinclude entries for {}",
            target.display()
        );
    }
    Ok(())
}

fn marker_path(target: &Path) -> PathBuf {
    let mut name = target.as_os_str().to_owned();
    name.push(COMPLETED_SUFFIX);
    PathBuf::from(name)
}

async fn read_marker(path: &Path) -> Option<Marker> {
    let contents = tokio::fs::read_to_string(path).await.ok()?;
    let mut lines = contents.lines();
    let branch = lines.next()?.trim().to_owned();
    if branch.is_empty() {
        return None;
    }
    let base = lines
        .next()
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty());
    Some(Marker { branch, base })
}

async fn write_marker(path: &Path, branch: &str, base: &str) -> Result<(), Failure> {
    let contents = format!("{branch}\n{base}\n");
    tokio::fs::write(path, contents).await.map_err(|error| {
        Failure::new(
            "permission_denied",
            format!("could not record {}: {error}", path.display()),
        )
    })
}

async fn is_directory(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
}

async fn is_git_repo(path: &Path) -> bool {
    matches!(
        run_git(
            path,
            vec!["rev-parse".into(), "--is-inside-work-tree".into()],
            GIT_OUTPUT_BYTES,
        )
        .await,
        Ok(output) if String::from_utf8_lossy(&output.stdout).trim() == "true"
    )
}

async fn has_commit(path: &Path) -> Option<String> {
    run_git(
        path,
        vec![
            "rev-parse".into(),
            "--verify".into(),
            "HEAD^{commit}".into(),
        ],
        GIT_OUTPUT_BYTES,
    )
    .await
    .ok()
    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

async fn is_on_branch(target: &Path, branch_name: &str) -> bool {
    match run_git(
        target,
        vec![
            "symbolic-ref".into(),
            "--quiet".into(),
            "--short".into(),
            "HEAD".into(),
        ],
        GIT_OUTPUT_BYTES,
    )
    .await
    {
        Ok(output) => String::from_utf8_lossy(&output.stdout).trim() == branch_name,
        Err(_) => false,
    }
}

/// The source repository's default branch, preferring `origin/HEAD`.
async fn resolve_default_branch(source: &Path) -> Option<String> {
    if let Ok(output) = run_git(
        source,
        vec![
            "symbolic-ref".into(),
            "--quiet".into(),
            "--short".into(),
            "refs/remotes/origin/HEAD".into(),
        ],
        GIT_OUTPUT_BYTES,
    )
    .await
    {
        let reference = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if let Some(name) = reference.strip_prefix("origin/") {
            if !name.is_empty() {
                return Some(name.to_owned());
            }
        }
    }
    run_git(
        source,
        vec![
            "symbolic-ref".into(),
            "--quiet".into(),
            "--short".into(),
            "HEAD".into(),
        ],
        GIT_OUTPUT_BYTES,
    )
    .await
    .ok()
    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    .filter(|name| !name.is_empty())
}

/// Resolves the base reference `git worktree add` is given.
///
/// A requested branch is tried as named, then as `origin/<name>`; the default
/// branch prefers the remote-tracking ref so a stale local branch does not
/// silently seed a worktree. No fetch happens here (see the module docs), so a
/// branch that exists only on the remote is refused with a reason.
async fn resolve_base(
    source: &Path,
    requested: Option<&str>,
    default_branch: Option<&str>,
) -> Result<String, Failure> {
    let candidates: Vec<String> = match requested {
        Some(requested) => {
            let mut candidates = vec![requested.to_owned()];
            if !requested.starts_with("origin/") {
                candidates.push(format!("origin/{requested}"));
            }
            candidates
        }
        None => match default_branch {
            Some(default) => vec![format!("origin/{default}"), default.to_owned()],
            None => Vec::new(),
        },
    };
    for candidate in &candidates {
        if reference_exists(source, candidate).await {
            return Ok(candidate.clone());
        }
    }
    Err(Failure::new(
        "base_branch_not_found",
        match requested {
            Some(requested) => format!(
                "base branch {requested} does not exist in {}",
                source.display()
            ),
            None => format!(
                "{} has no default branch to cut a worktree from",
                source.display()
            ),
        },
    ))
}

async fn reference_exists(source: &Path, reference: &str) -> bool {
    run_git(
        source,
        vec![
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            format!("{reference}^{{commit}}"),
        ],
        GIT_OUTPUT_BYTES,
    )
    .await
    .is_ok()
}

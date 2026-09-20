//! Host-side workspace and git operations.
//!
//! The server never opens an environment path. It sends a [`HostRpcRequest`]
//! to the worker that owns the environment, and this module performs the
//! operation on that machine. Every command is bounded and every path is
//! checked again here because the server cannot see host-local symlinks.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::Engine;
use loom_provider_protocol::{
    HostRpcOperation, HostRpcOutcome, HostRpcReport, HostRpcRequest, WorkspaceDiffFileSide,
    WorkspaceDiffTarget,
};
use serde_json::{json, Value};
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Maximum time allowed for one git child process.
pub const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// A command's per-stream memory ceiling, even when a request asks for more.
const MAX_COMMAND_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_COMMAND_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_BRANCH_LIMIT: usize = 100;
const DEFAULT_FILE_LIMIT: usize = 200;
const DEFAULT_UNTRACKED_LINE_FILES: usize = 100;
const DEFAULT_UNTRACKED_LINE_BYTES: u64 = 512 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct Failure {
    code: &'static str,
    pub(crate) message: String,
}

impl Failure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug)]
struct CommandOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

/// Answers one request. This function is async so command children can be
/// killed on timeout without blocking the worker's socket loop.
pub async fn answer(request: HostRpcRequest) -> HostRpcReport {
    answer_with_root(request, crate::default_environment_root()).await
}

/// Answers a request using the worker's configured workspace root.
pub async fn answer_with_root(
    request: HostRpcRequest,
    default_root: std::path::PathBuf,
) -> HostRpcReport {
    let host_id = request.host_id.clone();
    let request_id = request.request_id.clone();
    let outcome = match &request.operation {
        HostRpcOperation::PickFolder { client_host_id } => {
            if client_host_id.trim().is_empty() {
                Err(Failure::new("invalid_request", "client host id is empty"))
            } else {
                Ok(serde_json::json!({ "path": null }))
            }
        }
        HostRpcOperation::CloneDefaultPath { project_id } => Ok(serde_json::json!({
            "path": default_root.join(project_id.to_string()).to_string_lossy()
        })),
        // Defensive: `lib.rs` routes a history load to its own handler, and
        // this one must not try to open a workspace for it. Failing loudly is
        // better than answering a load request with a workspace result.
        HostRpcOperation::LoadHistory { .. } => Err(Failure::new(
            "wrong_operation",
            "a history load is not a workspace operation",
        )),
        _ => {
            let workspace_path = workspace_path(&request.operation).to_owned();
            match prepare_workspace(&workspace_path).await {
                Ok(workspace) => execute(workspace, request.operation).await,
                Err(error) => Err(error),
            }
        }
    };
    let outcome = match outcome {
        Ok(result) => HostRpcOutcome::Result { result },
        Err(error) => HostRpcOutcome::Failed {
            code: error.code.to_owned(),
            message: error.message,
        },
    };
    HostRpcReport {
        host_id,
        request_id,
        outcome,
    }
}

fn workspace_path(operation: &HostRpcOperation) -> &str {
    match operation {
        HostRpcOperation::InspectGitSource { path, .. }
        | HostRpcOperation::ListBranchOptions { path, .. }
        | HostRpcOperation::ListCommands { cwd: path } => path,
        HostRpcOperation::PickFolder { .. } | HostRpcOperation::CloneDefaultPath { .. } => "",
        // A history load is routed to its own handler before this function is
        // reached; it is not a workspace operation and owns no workspace path.
        HostRpcOperation::LoadHistory { .. } => "",
        HostRpcOperation::WorkspaceStatus {
            workspace_context, ..
        }
        | HostRpcOperation::WorkspaceDiff {
            workspace_context, ..
        }
        | HostRpcOperation::WorkspaceDiffFiles {
            workspace_context, ..
        }
        | HostRpcOperation::WorkspaceDiffPatch {
            workspace_context, ..
        }
        | HostRpcOperation::WorkspaceDiffFile {
            workspace_context, ..
        }
        | HostRpcOperation::WorkspacePullRequest {
            workspace_context, ..
        }
        | HostRpcOperation::WorkspaceCommit {
            workspace_context, ..
        }
        | HostRpcOperation::WorkspacePullRequestAction {
            workspace_context, ..
        } => &workspace_context.workspace_path,
    }
}

async fn prepare_workspace(raw: &str) -> Result<PathBuf, Failure> {
    if raw.is_empty() || raw.contains('\0') || !Path::new(raw).is_absolute() {
        return Err(Failure::new(
            "invalid_path",
            format!("workspace path is not an absolute path: {raw:?}"),
        ));
    }
    let path = tokio::fs::canonicalize(raw).await.map_err(|error| {
        Failure::new(
            "path_not_found",
            format!("workspace path {raw:?} could not be opened: {error}"),
        )
    })?;
    let metadata = tokio::fs::metadata(&path).await.map_err(|error| {
        Failure::new(
            "path_not_found",
            format!(
                "workspace path {} could not be inspected: {error}",
                path.display()
            ),
        )
    })?;
    if !metadata.is_dir() {
        return Err(Failure::new(
            "workspace_type_mismatch",
            format!("workspace path {} is not a directory", path.display()),
        ));
    }
    Ok(path)
}

async fn execute(workspace: PathBuf, operation: HostRpcOperation) -> Result<Value, Failure> {
    validate_operation(&operation)?;
    match operation {
        HostRpcOperation::InspectGitSource { .. } => inspect_git_source(&workspace).await,
        HostRpcOperation::ListBranchOptions {
            limit,
            query,
            selected_branch,
            ..
        } => {
            list_branch_options(
                &workspace,
                limit,
                query.as_deref(),
                selected_branch.as_deref(),
            )
            .await
        }
        HostRpcOperation::WorkspaceStatus {
            merge_base_branch,
            max_untracked_line_stat_files,
            max_untracked_line_stat_bytes,
            ..
        } => {
            workspace_status(
                &workspace,
                merge_base_branch.as_deref(),
                max_untracked_line_stat_files,
                max_untracked_line_stat_bytes,
            )
            .await
        }
        HostRpcOperation::WorkspaceDiff {
            target,
            max_diff_bytes,
            max_file_list_bytes,
            max_untracked_files,
            ..
        } => {
            workspace_diff(
                &workspace,
                &target,
                max_diff_bytes,
                max_file_list_bytes,
                max_untracked_files,
            )
            .await
        }
        HostRpcOperation::WorkspaceDiffFiles {
            target, max_files, ..
        } => workspace_diff_files(&workspace, &target, max_files).await,
        HostRpcOperation::WorkspaceDiffPatch {
            target,
            paths,
            max_bytes_per_file,
            ..
        } => workspace_diff_patch(&workspace, &target, &paths, max_bytes_per_file).await,
        HostRpcOperation::WorkspaceDiffFile {
            target,
            path,
            side,
            max_bytes,
            ..
        } => workspace_diff_file(&workspace, &target, &path, side, max_bytes).await,
        HostRpcOperation::WorkspacePullRequest { .. } => Err(Failure::new(
            "pull_request_unavailable",
            "pull request metadata is unavailable: no host provider is configured",
        )),
        HostRpcOperation::WorkspaceCommit { message, .. } => {
            workspace_commit(&workspace, &message).await
        }
        HostRpcOperation::WorkspacePullRequestAction { .. } => Err(Failure::new(
            "pull_request_unavailable",
            "pull request actions are unavailable: no host provider is configured",
        )),
        HostRpcOperation::PickFolder { .. } | HostRpcOperation::CloneDefaultPath { .. } => {
            Ok(serde_json::json!({ "path": null }))
        }
        HostRpcOperation::ListCommands { cwd } => list_commands(&cwd).await,
        // Defensive, as above: `lib.rs` never routes a load here.
        HostRpcOperation::LoadHistory { .. } => Err(Failure::new(
            "wrong_operation",
            "a history load is not a workspace operation",
        )),
    }
}

fn validate_operation(operation: &HostRpcOperation) -> Result<(), Failure> {
    let invalid_target = || Failure::new("unknown", "invalid workspace diff target");
    match operation {
        HostRpcOperation::InspectGitSource { .. } => Ok(()),
        // Not validated here: a load is answered by its own handler, which
        // checks the session against the agent.
        HostRpcOperation::LoadHistory { .. } => Ok(()),
        HostRpcOperation::ListBranchOptions {
            query,
            selected_branch,
            ..
        } => {
            if let Some(query) = query {
                validate_query(query)?;
            }
            if let Some(branch) = selected_branch {
                validate_branch_reference(branch).map_err(|message| {
                    Failure::new(
                        "invalid_path",
                        format!("invalid selected branch: {message}"),
                    )
                })?;
            }
            Ok(())
        }
        HostRpcOperation::WorkspaceStatus {
            merge_base_branch, ..
        } => {
            if let Some(branch) = merge_base_branch {
                validate_branch_reference(branch).map_err(|message| {
                    Failure::new("unknown", format!("invalid merge-base branch: {message}"))
                })?;
            }
            Ok(())
        }
        HostRpcOperation::WorkspaceDiff { target, .. }
        | HostRpcOperation::WorkspaceDiffFiles { target, .. }
        | HostRpcOperation::WorkspaceDiffPatch { target, .. }
        | HostRpcOperation::WorkspaceDiffFile { target, .. } => {
            validate_diff_target(target).map_err(|_| invalid_target())?;
            match operation {
                HostRpcOperation::WorkspaceDiffPatch { paths, .. } => {
                    if paths.is_empty() || paths.len() > 50 {
                        return Err(Failure::new(
                            "invalid_request",
                            "a diff patch needs between one and fifty paths",
                        ));
                    }
                    for path in paths {
                        validate_relative_path(path)?;
                    }
                }
                HostRpcOperation::WorkspaceDiffFile { path, .. } => {
                    validate_relative_path(path)?;
                }
                _ => {}
            }
            Ok(())
        }
        HostRpcOperation::WorkspacePullRequest { .. } => Ok(()),
        HostRpcOperation::PickFolder { client_host_id } => {
            if client_host_id.trim().is_empty() {
                Err(Failure::new("invalid_request", "client host id is empty"))
            } else {
                Ok(())
            }
        }
        HostRpcOperation::CloneDefaultPath { .. } => Ok(()),
        HostRpcOperation::ListCommands { cwd } => {
            if cwd.is_empty() || cwd.contains('\0') || !Path::new(cwd).is_absolute() {
                Err(Failure::new(
                    "invalid_path",
                    format!("commands cwd is not an absolute path: {cwd:?}"),
                ))
            } else {
                Ok(())
            }
        }
        HostRpcOperation::WorkspaceCommit { message, .. } => {
            if message.trim().is_empty() {
                Err(Failure::new(
                    "invalid_request",
                    "commit message must not be empty",
                ))
            } else {
                Ok(())
            }
        }
        HostRpcOperation::WorkspacePullRequestAction {
            operation, method, ..
        } => {
            let valid_operation = matches!(
                operation.as_str(),
                "pull_request_ready" | "pull_request_draft" | "pull_request_merge"
            );
            let valid_method = method
                .as_deref()
                .is_none_or(|method| matches!(method, "merge" | "squash" | "rebase"));
            if valid_operation && valid_method {
                Ok(())
            } else {
                Err(Failure::new(
                    "pull_request_action_failed",
                    "invalid pull request action",
                ))
            }
        }
    }
}

fn validate_query(query: &str) -> Result<(), Failure> {
    if !(1..=256).contains(&query.chars().count()) {
        return Err(Failure::new(
            "invalid_request",
            "query must contain between one and 256 characters",
        ));
    }
    Ok(())
}

fn validate_diff_target(target: &WorkspaceDiffTarget) -> Result<(), Failure> {
    match target {
        WorkspaceDiffTarget::Uncommitted => Ok(()),
        WorkspaceDiffTarget::BranchCommitted { merge_base_branch }
        | WorkspaceDiffTarget::All { merge_base_branch } => {
            validate_branch_reference(merge_base_branch)
                .map_err(|message| Failure::new("unknown", message))
        }
        WorkspaceDiffTarget::Commit { sha } if valid_hex_revision(sha) => Ok(()),
        WorkspaceDiffTarget::Commit { .. } => Err(Failure::new(
            "unknown",
            "commit must name a hexadecimal revision",
        )),
    }
}

fn valid_hex_revision(raw: &str) -> bool {
    (4..=40).contains(&raw.len())
        && raw
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn validate_branch_reference(raw: &str) -> Result<(), &'static str> {
    if raw.is_empty() {
        return Err("branch is empty");
    }
    if raw.len() > 4_096 {
        return Err("branch is too long");
    }
    if raw == "@" || raw.starts_with('-') {
        return Err("branch is not a valid reference");
    }
    if raw
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err("branch contains whitespace or control characters");
    }
    if raw.contains("..") || raw.contains("@{") || raw.starts_with('/') || raw.ends_with('/') {
        return Err("branch contains a forbidden reference sequence");
    }
    if raw
        .bytes()
        .any(|byte| matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\'))
    {
        return Err("branch contains a forbidden reference character");
    }
    if raw.split('/').any(|component| {
        component.is_empty()
            || component == "."
            || component == ".."
            || component.starts_with('.')
            || component.ends_with('.')
            || component.ends_with(".lock")
    }) {
        return Err("branch contains an invalid reference component");
    }
    Ok(())
}

async fn inspect_git_source(workspace: &Path) -> Result<Value, Failure> {
    let repo = inspect_repo(workspace).await?;
    let local_candidates = ref_names(workspace, "refs/heads").await?;
    let remote_candidates = ref_names(workspace, "refs/remotes").await?;
    let branches_truncated = local_candidates.len() > DEFAULT_BRANCH_LIMIT;
    let remote_branches_truncated = remote_candidates.len() > DEFAULT_BRANCH_LIMIT;
    let branches = local_candidates
        .into_iter()
        .take(DEFAULT_BRANCH_LIMIT)
        .collect::<Vec<_>>();
    let remote_branches = remote_candidates
        .into_iter()
        .take(DEFAULT_BRANCH_LIMIT)
        .collect::<Vec<_>>();
    Ok(json!({
        "checkout": repo.checkout,
        "defaultBranch": repo.default_branch,
        "isWorktree": repo.is_worktree,
        "defaultBranchRelation": repo.default_branch_relation,
        "hasUncommittedChanges": repo.has_uncommitted_changes,
        "operation": { "kind": "none" },
        "originDefaultBranch": repo.origin_default_branch,
        "branches": branches,
        "branchesTruncated": branches_truncated,
        "remoteBranches": remote_branches,
        "remoteBranchesTruncated": remote_branches_truncated,
        "selectedBranch": null,
        "defaultWorktreeBaseBranch": repo.default_branch,
    }))
}

async fn list_branch_options(
    workspace: &Path,
    limit: usize,
    query: Option<&str>,
    selected_branch: Option<&str>,
) -> Result<Value, Failure> {
    let locals = ref_names(workspace, "refs/heads").await?;
    let remotes = ref_names(workspace, "refs/remotes").await?;
    let query = query.map(str::to_ascii_lowercase);
    let matches = |name: &str| {
        query
            .as_deref()
            .is_none_or(|query| name.to_ascii_lowercase().contains(query))
    };
    let local_candidates: Vec<String> = locals.into_iter().filter(|name| matches(name)).collect();
    let remote_candidates: Vec<String> = remotes.into_iter().filter(|name| matches(name)).collect();
    let limit = if limit == 0 {
        DEFAULT_BRANCH_LIMIT
    } else {
        limit.min(1_000)
    };
    let branches_truncated = local_candidates.len() > limit;
    let remote_branches_truncated = remote_candidates.len() > limit;
    let branches = local_candidates.into_iter().take(limit).collect::<Vec<_>>();
    let remote_branches = remote_candidates
        .into_iter()
        .take(limit)
        .collect::<Vec<_>>();
    let selected_branch = match selected_branch.filter(|branch| !branch.is_empty()) {
        Some(branch) => {
            let kind = if branches.iter().any(|candidate| candidate == branch)
                || ref_exists(workspace, branch, "refs/heads").await?
            {
                "local"
            } else if remote_branches.iter().any(|candidate| candidate == branch)
                || ref_exists(workspace, branch, "refs/remotes").await?
            {
                "remote"
            } else {
                "missing"
            };
            Some(json!({ "name": branch, "kind": kind }))
        }
        None => None,
    };
    Ok(json!({
        "branches": branches,
        "branchesTruncated": branches_truncated,
        "remoteBranches": remote_branches,
        "remoteBranchesTruncated": remote_branches_truncated,
        "selectedBranch": selected_branch,
    }))
}

async fn ref_names(workspace: &Path, namespace: &str) -> Result<Vec<String>, Failure> {
    let output = run_git(
        workspace,
        vec![
            "for-each-ref".into(),
            "--format=%(refname:short)".into(),
            namespace.into(),
        ],
        DEFAULT_COMMAND_OUTPUT_BYTES,
    )
    .await?;
    ensure_complete(&output, "git branch listing")?;
    let mut names = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .filter(|name| !name.ends_with("/HEAD"))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    names.sort();
    Ok(names)
}

async fn ref_exists(workspace: &Path, name: &str, namespace: &str) -> Result<bool, Failure> {
    let reference = if name.starts_with("refs/") {
        name.to_owned()
    } else {
        format!("{namespace}/{name}")
    };
    Ok(run_git(
        workspace,
        vec!["show-ref".into(), "--verify".into(), reference],
        DEFAULT_COMMAND_OUTPUT_BYTES,
    )
    .await
    .is_ok())
}

struct RepoInfo {
    checkout: Value,
    current_branch: Option<String>,
    default_branch: Option<String>,
    origin_default_branch: Option<String>,
    default_branch_relation: Option<String>,
    is_worktree: bool,
    has_uncommitted_changes: bool,
}

async fn inspect_repo(workspace: &Path) -> Result<RepoInfo, Failure> {
    let head_sha = git_optional(
        workspace,
        vec!["rev-parse".into(), "--verify".into(), "HEAD".into()],
        200,
    )
    .await?;
    let current_branch = git_optional(
        workspace,
        vec![
            "symbolic-ref".into(),
            "--quiet".into(),
            "--short".into(),
            "HEAD".into(),
        ],
        200,
    )
    .await?;
    let checkout = if let Some(branch) = current_branch.clone() {
        json!({ "kind": "branch", "branchName": branch, "headSha": head_sha })
    } else if let Some(head_sha) = head_sha.clone() {
        json!({ "kind": "detached", "headSha": head_sha })
    } else {
        let unborn_branch = git_optional(
            workspace,
            vec![
                "symbolic-ref".into(),
                "--quiet".into(),
                "--short".into(),
                "HEAD".into(),
            ],
            200,
        )
        .await?;
        json!({ "kind": "unborn", "branchName": unborn_branch })
    };
    let origin_default_branch = git_optional(
        workspace,
        vec![
            "symbolic-ref".into(),
            "--quiet".into(),
            "--short".into(),
            "refs/remotes/origin/HEAD".into(),
        ],
        300,
    )
    .await?
    .and_then(|value| {
        value
            .strip_prefix("origin/")
            .map(str::to_owned)
            .or(Some(value))
    });
    let default_branch = origin_default_branch
        .clone()
        .or_else(|| current_branch.clone());
    let default_branch_relation = match (&current_branch, &default_branch, &head_sha) {
        (Some(current), Some(default), Some(_)) if current == default => Some("equal".into()),
        (Some(_), Some(default), Some(_)) => branch_relation(workspace, default).await?,
        _ => None,
    };
    let has_uncommitted_changes = !git_status(workspace).await?.is_empty();
    let is_worktree = is_worktree(workspace).await?;
    Ok(RepoInfo {
        checkout,
        current_branch,
        default_branch,
        origin_default_branch,
        default_branch_relation,
        is_worktree,
        has_uncommitted_changes,
    })
}

async fn branch_relation(workspace: &Path, base: &str) -> Result<Option<String>, Failure> {
    let Some(head) = git_optional(
        workspace,
        vec!["rev-parse".into(), "--verify".into(), "HEAD".into()],
        200,
    )
    .await?
    else {
        return Ok(None);
    };
    let Some(counts) = git_optional(
        workspace,
        vec![
            "rev-list".into(),
            "--left-right".into(),
            "--count".into(),
            format!("{base}...{head}"),
        ],
        200,
    )
    .await?
    else {
        return Ok(Some("unknown".into()));
    };
    let mut fields = counts
        .split_whitespace()
        .filter_map(|value| value.parse::<u64>().ok());
    let behind = fields.next().unwrap_or(0);
    let ahead = fields.next().unwrap_or(0);
    Ok(Some(
        match (behind, ahead) {
            (0, 0) => "equal",
            (0, _) => "local-ahead",
            (_, 0) => "local-behind",
            _ => "diverged",
        }
        .into(),
    ))
}

async fn is_worktree(workspace: &Path) -> Result<bool, Failure> {
    let git_dir = git_optional(
        workspace,
        vec!["rev-parse".into(), "--git-dir".into()],
        DEFAULT_COMMAND_OUTPUT_BYTES,
    )
    .await?;
    let common_dir = git_optional(
        workspace,
        vec!["rev-parse".into(), "--git-common-dir".into()],
        DEFAULT_COMMAND_OUTPUT_BYTES,
    )
    .await?;
    Ok(git_dir
        .zip(common_dir)
        .is_some_and(|(git, common)| git != common))
}

#[derive(Clone, Debug)]
struct StatusEntry {
    path: String,
    status: String,
    origin: &'static str,
}

async fn git_status(workspace: &Path) -> Result<Vec<StatusEntry>, Failure> {
    let output = run_git(
        workspace,
        vec![
            "status".into(),
            "--porcelain=v1".into(),
            "--untracked-files=all".into(),
        ],
        DEFAULT_COMMAND_OUTPUT_BYTES,
    )
    .await?;
    ensure_complete(&output, "git status")?;
    Ok(parse_status(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_status(raw: &str) -> Vec<StatusEntry> {
    raw.lines()
        .filter_map(|line| {
            if line.len() < 3 {
                return None;
            }
            let code = &line[..2];
            let path = line[3..].trim().to_owned();
            if path.is_empty() {
                return None;
            }
            if code == "??" {
                return Some(StatusEntry {
                    path,
                    status: "??".into(),
                    origin: "untracked",
                });
            }
            let path = if code.contains('R') || code.contains('C') {
                match path.split_once(" -> ") {
                    Some((_, new)) => new.to_owned(),
                    None => path,
                }
            } else {
                path
            };
            let status = status_letter(code);
            Some(StatusEntry {
                path,
                status,
                origin: "tracked",
            })
        })
        .collect()
}

fn status_letter(code: &str) -> String {
    if code.contains('U') {
        return "U".into();
    }
    let first = code.as_bytes().first().copied().unwrap_or(b' ');
    let second = code.as_bytes().get(1).copied().unwrap_or(b' ');
    let selected = if first != b' ' { first } else { second };
    match selected as char {
        'M' | 'A' | 'D' | 'R' | 'C' => (selected as char).to_string(),
        _ => "M".into(),
    }
}

async fn workspace_status(
    workspace: &Path,
    merge_base_branch: Option<&str>,
    max_untracked_line_stat_files: u64,
    max_untracked_line_stat_bytes: u64,
) -> Result<Value, Failure> {
    let repo = inspect_repo(workspace).await?;
    let status = git_status(workspace).await?;
    let max_files = bounded_count(max_untracked_line_stat_files, DEFAULT_UNTRACKED_LINE_FILES);
    let max_bytes = if max_untracked_line_stat_bytes == 0 {
        DEFAULT_UNTRACKED_LINE_BYTES
    } else {
        max_untracked_line_stat_bytes.min(16 * 1024 * 1024)
    };
    let stats = numstat(workspace, &WorkspaceDiffTarget::Uncommitted).await?;
    let mut working_files = Vec::with_capacity(status.len());
    let mut insertions = 0u64;
    let mut deletions = 0u64;
    let mut line_stats_complete = true;
    let mut untracked_seen = 0usize;
    for entry in &status {
        let (added, deleted) = if entry.origin == "untracked" {
            untracked_seen += 1;
            if untracked_seen > max_files {
                line_stats_complete = false;
                (0, 0)
            } else {
                match untracked_line_stats(workspace, &entry.path, max_bytes).await {
                    Ok((added, deleted)) => (added, deleted),
                    Err(error) if error.code == "file_too_large" => {
                        line_stats_complete = false;
                        (0, 0)
                    }
                    Err(_) => {
                        line_stats_complete = false;
                        (0, 0)
                    }
                }
            }
        } else {
            stats.get(&entry.path).copied().unwrap_or((0, 0))
        };
        insertions = insertions.saturating_add(added);
        deletions = deletions.saturating_add(deleted);
        working_files.push(json!({
            "path": entry.path,
            "status": entry.status,
            "insertions": added,
            "deletions": deleted,
        }));
    }
    let branch = json!({
        "currentBranch": repo.current_branch,
        "defaultBranch": repo.default_branch.clone().unwrap_or_default(),
    });
    let committed = committed_merge_base(
        workspace,
        merge_base_branch.or(repo.default_branch.as_deref()),
    )
    .await?;
    let has_committed_unmerged_changes = committed
        .as_ref()
        .and_then(|value| value.get("hasCommittedUnmergedChanges"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let state = match (
        status.iter().any(|entry| entry.origin == "untracked"),
        !status.is_empty(),
        has_committed_unmerged_changes,
    ) {
        (_, false, false) => "clean",
        (true, _, true) => "dirty_and_committed_unmerged",
        (true, _, false) => "untracked",
        (false, true, true) => "dirty_and_committed_unmerged",
        (false, true, false) => "dirty_uncommitted",
        (false, false, true) => "committed_unmerged",
    };
    Ok(json!({
        "outcome": "available",
        "workspace": {
            "workingTree": {
                "insertions": insertions,
                "deletions": deletions,
                "lineStatsComplete": line_stats_complete,
                "files": working_files,
                "hasUncommittedChanges": !status.is_empty(),
                "state": state,
            },
            "checkout": repo.checkout,
            "branch": branch,
            "mergeBase": committed,
        }
    }))
}

async fn untracked_line_stats(
    workspace: &Path,
    relative: &str,
    max_bytes: u64,
) -> Result<(u64, u64), Failure> {
    let path = safe_workspace_path(workspace, relative).await?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|error| Failure::new("path_not_found", format!("{}: {error}", path.display())))?;
    if !metadata.is_file() {
        return Ok((0, 0));
    }
    if metadata.len() > max_bytes {
        return Err(Failure::new(
            "file_too_large",
            format!("untracked file {relative:?} exceeds the line-stat limit"),
        ));
    }
    let file = tokio::fs::File::open(path).await.map_err(|error| {
        Failure::new(
            "permission_denied",
            format!("could not read {relative:?}: {error}"),
        )
    })?;
    let mut bytes = Vec::with_capacity(metadata.len().min(max_bytes) as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| {
            Failure::new(
                "permission_denied",
                format!("could not read {relative:?}: {error}"),
            )
        })?;
    if bytes.len() as u64 > max_bytes {
        return Err(Failure::new(
            "file_too_large",
            format!("untracked file {relative:?} exceeds the line-stat limit"),
        ));
    }
    let lines = bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
    Ok((if bytes.is_empty() { 0 } else { lines.max(1) }, 0))
}

async fn committed_merge_base(
    workspace: &Path,
    branch: Option<&str>,
) -> Result<Option<Value>, Failure> {
    let Some(branch) = branch.filter(|branch| !branch.trim().is_empty()) else {
        return Ok(None);
    };
    let Some(base_ref) = git_optional(
        workspace,
        vec!["merge-base".into(), "HEAD".into(), branch.into()],
        DEFAULT_COMMAND_OUTPUT_BYTES,
    )
    .await?
    else {
        return Ok(None);
    };
    let Some(counts) = git_optional(
        workspace,
        vec![
            "rev-list".into(),
            "--left-right".into(),
            "--count".into(),
            format!("{branch}...HEAD"),
        ],
        200,
    )
    .await?
    else {
        return Ok(None);
    };
    let mut fields = counts
        .split_whitespace()
        .filter_map(|field| field.parse::<u64>().ok());
    let behind = fields.next().unwrap_or(0);
    let ahead = fields.next().unwrap_or(0);
    let target = WorkspaceDiffTarget::BranchCommitted {
        merge_base_branch: branch.to_owned(),
    };
    let entries = diff_entries(workspace, &target, DEFAULT_FILE_LIMIT).await?;
    let stats = entries
        .iter()
        .fold((0u64, 0u64), |(added, deleted), entry| {
            (
                added.saturating_add(entry.additions.unwrap_or(0)),
                deleted.saturating_add(entry.deletions.unwrap_or(0)),
            )
        });
    let commits = commits_since(workspace, branch).await?;
    Ok(Some(json!({
        "insertions": stats.0,
        "deletions": stats.1,
        "lineStatsComplete": true,
        "files": entries.iter().map(|entry| json!({
            "path": entry.path,
            "status": entry.status,
            "insertions": entry.additions,
            "deletions": entry.deletions,
        })).collect::<Vec<_>>(),
        "mergeBaseBranch": branch,
        "baseRef": base_ref,
        "aheadCount": ahead,
        "behindCount": behind,
        "hasCommittedUnmergedChanges": ahead > 0,
        "commits": commits,
    })))
}

async fn commits_since(workspace: &Path, branch: &str) -> Result<Vec<Value>, Failure> {
    let output = run_git(
        workspace,
        vec![
            "log".into(),
            "--format=%H%x09%h%x09%s%x09%an%x09%at".into(),
            format!("{branch}..HEAD"),
        ],
        DEFAULT_COMMAND_OUTPUT_BYTES,
    )
    .await?;
    ensure_complete(&output, "git commit listing")?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(5, '\t');
            let sha = fields.next()?.to_owned();
            let short_sha = fields.next()?.to_owned();
            let subject = fields.next()?.to_owned();
            let author_name = fields.next()?.to_owned();
            let authored_at = fields.next()?.parse::<u64>().ok()?.saturating_mul(1_000);
            Some(json!({
                "sha": sha,
                "shortSha": short_sha,
                "subject": subject,
                "authorName": author_name,
                "authoredAt": authored_at,
            }))
        })
        .collect())
}

async fn workspace_diff(
    workspace: &Path,
    target: &WorkspaceDiffTarget,
    max_diff_bytes: u64,
    max_file_list_bytes: u64,
    max_untracked_files: u64,
) -> Result<Value, Failure> {
    let diff_limit = bounded_bytes(max_diff_bytes);
    let file_limit = bounded_bytes(max_file_list_bytes);
    let args = diff_args(target, "diff");
    let diff_output = run_git(workspace, args, diff_limit).await?;
    let files_output = run_git(workspace, diff_args(target, "name-status"), file_limit).await?;
    let mut files = String::from_utf8_lossy(&files_output.stdout).into_owned();
    let mut truncated = diff_output.stdout_truncated
        || diff_output.stderr_truncated
        || files_output.stdout_truncated
        || files_output.stderr_truncated;
    let max_untracked = bounded_count(max_untracked_files, DEFAULT_FILE_LIMIT);
    if matches!(
        target,
        WorkspaceDiffTarget::Uncommitted | WorkspaceDiffTarget::All { .. }
    ) {
        let untracked_entries = git_status(workspace)
            .await?
            .into_iter()
            .filter(|entry| entry.origin == "untracked")
            .collect::<Vec<_>>();
        truncated |= untracked_entries.len() > max_untracked;
        let untracked = untracked_entries
            .into_iter()
            .take(max_untracked)
            .map(|entry| format!("??\t{}\n", entry.path))
            .collect::<String>();
        if !untracked.is_empty() {
            files.push_str(&untracked);
        }
    }
    let shortstat_output = run_git(
        workspace,
        diff_args(target, "shortstat"),
        DEFAULT_COMMAND_OUTPUT_BYTES,
    )
    .await?;
    ensure_complete(&shortstat_output, "git diff summary")?;
    let shortstat = String::from_utf8_lossy(&shortstat_output.stdout)
        .trim()
        .to_owned();
    let merge_base_ref = merge_base_ref(workspace, target).await?;
    if files.len() > file_limit {
        truncate_to_char_boundary(&mut files, file_limit);
        truncated = true;
    }
    Ok(json!({
        "outcome": "available",
        "diff": {
            "diff": String::from_utf8_lossy(&diff_output.stdout),
            "truncated": truncated,
            "shortstat": shortstat,
            "files": files,
            "mergeBaseRef": merge_base_ref,
        }
    }))
}

async fn workspace_diff_files(
    workspace: &Path,
    target: &WorkspaceDiffTarget,
    max_files: u64,
) -> Result<Value, Failure> {
    let limit = bounded_count(max_files, DEFAULT_FILE_LIMIT);
    let entries = diff_entries(workspace, target, limit).await?;
    let all_entries =
        diff_entries(workspace, target, DEFAULT_FILE_LIMIT.saturating_mul(20)).await?;
    let truncated = all_entries.len() > entries.len();
    let shortstat_output = run_git(
        workspace,
        diff_args(target, "shortstat"),
        DEFAULT_COMMAND_OUTPUT_BYTES,
    )
    .await?;
    ensure_complete(&shortstat_output, "git diff summary")?;
    let shortstat = String::from_utf8_lossy(&shortstat_output.stdout)
        .trim()
        .to_owned();
    let merge_base_ref = merge_base_ref(workspace, target).await?;
    Ok(json!({
        "outcome": "available",
        "files": entries.iter().map(diff_file_value).collect::<Vec<_>>(),
        "truncated": truncated,
        "shortstat": shortstat,
        "mergeBaseRef": merge_base_ref,
        "initialPatches": [],
    }))
}

#[derive(Clone, Debug)]
struct DiffEntry {
    path: String,
    previous_path: Option<String>,
    status: String,
    additions: Option<u64>,
    deletions: Option<u64>,
    binary: bool,
    origin: &'static str,
}

fn diff_file_value(entry: &DiffEntry) -> Value {
    let change_kind = match entry.status.as_str() {
        "A" | "??" => "added",
        "D" => "deleted",
        "R" => "renamed",
        "C" => "copied",
        "U" => "type_changed",
        _ => "modified",
    };
    let load_mode = if entry.binary { "on_demand" } else { "auto" };
    json!({
        "path": entry.path,
        "previousPath": entry.previous_path,
        "changeKind": change_kind,
        "additions": entry.additions.unwrap_or(0),
        "deletions": entry.deletions.unwrap_or(0),
        "binary": entry.binary,
        "origin": entry.origin,
        "loadMode": load_mode,
    })
}

async fn diff_entries(
    workspace: &Path,
    target: &WorkspaceDiffTarget,
    limit: usize,
) -> Result<Vec<DiffEntry>, Failure> {
    let output = run_git(
        workspace,
        diff_args(target, "name-status"),
        DEFAULT_COMMAND_OUTPUT_BYTES,
    )
    .await?;
    let mut entries = parse_name_status(&String::from_utf8_lossy(&output.stdout));
    if matches!(
        target,
        WorkspaceDiffTarget::Uncommitted | WorkspaceDiffTarget::All { .. }
    ) {
        for status in git_status(workspace).await? {
            if status.origin == "untracked"
                && !entries.iter().any(|entry| entry.path == status.path)
            {
                entries.push(DiffEntry {
                    path: status.path,
                    previous_path: None,
                    status: "??".into(),
                    additions: None,
                    deletions: None,
                    binary: false,
                    origin: "untracked",
                });
            }
        }
    }
    let stats = numstat(workspace, target).await?;
    for entry in &mut entries {
        if let Some((added, deleted)) = stats.get(&entry.path) {
            entry.additions = Some(*added);
            entry.deletions = Some(*deleted);
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    entries.truncate(limit);
    Ok(entries)
}

fn parse_name_status(raw: &str) -> Vec<DiffEntry> {
    raw.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let status_field = fields.next()?.trim();
            if status_field.is_empty() {
                return None;
            }
            let status = status_field.chars().next()?.to_string();
            let first = fields.next()?.to_owned();
            let (previous_path, path) = if status == "R" || status == "C" {
                (Some(first), fields.next()?.to_owned())
            } else {
                (None, first)
            };
            Some(DiffEntry {
                path,
                previous_path,
                status,
                additions: None,
                deletions: None,
                binary: false,
                origin: "tracked",
            })
        })
        .collect()
}

async fn numstat(
    workspace: &Path,
    target: &WorkspaceDiffTarget,
) -> Result<HashMap<String, (u64, u64)>, Failure> {
    let output = run_git(
        workspace,
        diff_args(target, "numstat"),
        DEFAULT_COMMAND_OUTPUT_BYTES,
    )
    .await?;
    ensure_complete(&output, "git diff statistics")?;
    let mut stats = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split('\t');
        let additions = fields.next();
        let deletions = fields.next();
        let path = fields.next();
        let (Some(additions), Some(deletions), Some(path)) = (additions, deletions, path) else {
            continue;
        };
        let binary = additions == "-" || deletions == "-";
        stats.insert(
            path.to_owned(),
            if binary {
                (0, 0)
            } else {
                (
                    additions.parse().unwrap_or(0),
                    deletions.parse().unwrap_or(0),
                )
            },
        );
    }
    Ok(stats)
}

fn diff_args(target: &WorkspaceDiffTarget, format: &str) -> Vec<String> {
    let option = (format != "diff").then(|| format!("--{format}"));
    match target {
        WorkspaceDiffTarget::Uncommitted => {
            let mut args = vec!["diff".into()];
            if let Some(option) = option {
                args.push(option);
            }
            args.extend(["HEAD".into(), "--".into()]);
            args
        }
        WorkspaceDiffTarget::BranchCommitted { merge_base_branch } => {
            let mut args = vec!["diff".into()];
            if let Some(option) = option {
                args.push(option);
            }
            args.extend([format!("{merge_base_branch}...HEAD"), "--".into()]);
            args
        }
        WorkspaceDiffTarget::All { merge_base_branch } => {
            let mut args = vec!["diff".into()];
            if let Some(option) = option {
                args.push(option);
            }
            args.extend([merge_base_branch.clone(), "--".into()]);
            args
        }
        WorkspaceDiffTarget::Commit { sha } => {
            let mut args = vec!["show".into()];
            if let Some(option) = option {
                args.push(option);
            }
            args.extend(["--format=".into(), sha.clone(), "--".into()]);
            args
        }
    }
}

async fn merge_base_ref(
    workspace: &Path,
    target: &WorkspaceDiffTarget,
) -> Result<Option<String>, Failure> {
    let branch = match target {
        WorkspaceDiffTarget::BranchCommitted { merge_base_branch }
        | WorkspaceDiffTarget::All { merge_base_branch } => Some(merge_base_branch.as_str()),
        WorkspaceDiffTarget::Uncommitted | WorkspaceDiffTarget::Commit { .. } => None,
    };
    let Some(branch) = branch else {
        return Ok(None);
    };
    git_optional(
        workspace,
        vec!["merge-base".into(), "HEAD".into(), branch.into()],
        DEFAULT_COMMAND_OUTPUT_BYTES,
    )
    .await
}

async fn workspace_diff_patch(
    workspace: &Path,
    target: &WorkspaceDiffTarget,
    paths: &[String],
    max_bytes_per_file: u64,
) -> Result<Value, Failure> {
    if paths.is_empty() || paths.len() > 50 {
        return Err(Failure::new(
            "unknown",
            "a diff patch needs between one and fifty paths",
        ));
    }
    let limit = bounded_bytes(max_bytes_per_file);
    let mut patches = Vec::with_capacity(paths.len());
    for path in paths {
        validate_relative_path(path)?;
        let mut args = diff_args(target, "patch");
        args.push(path.clone());
        let output = run_git(workspace, args, limit).await?;
        patches.push(json!({
            "path": path,
            "patch": String::from_utf8_lossy(&output.stdout),
            "truncated": output.stdout_truncated || output.stderr_truncated,
        }));
    }
    Ok(json!({ "outcome": "available", "patches": patches }))
}

async fn workspace_diff_file(
    workspace: &Path,
    target: &WorkspaceDiffTarget,
    path: &str,
    side: WorkspaceDiffFileSide,
    max_bytes: u64,
) -> Result<Value, Failure> {
    validate_relative_path(path)?;
    let limit = bounded_bytes(max_bytes);
    let bytes = match (target, side) {
        (WorkspaceDiffTarget::Uncommitted, WorkspaceDiffFileSide::New)
        | (WorkspaceDiffTarget::All { .. }, WorkspaceDiffFileSide::New) => {
            let path_on_disk = safe_workspace_path(workspace, path).await?;
            read_optional_file(&path_on_disk, limit).await?
        }
        (WorkspaceDiffTarget::Uncommitted, WorkspaceDiffFileSide::Old) => {
            git_file_or_empty(workspace, "HEAD", path, limit).await?
        }
        (
            WorkspaceDiffTarget::BranchCommitted { merge_base_branch },
            WorkspaceDiffFileSide::Old,
        ) => {
            let base = git_optional(
                workspace,
                vec![
                    "merge-base".into(),
                    "HEAD".into(),
                    merge_base_branch.clone(),
                ],
                DEFAULT_COMMAND_OUTPUT_BYTES,
            )
            .await?
            .ok_or_else(|| {
                Failure::new("unknown", "the merge-base branch could not be resolved")
            })?;
            git_file_or_empty(workspace, &base, path, limit).await?
        }
        (WorkspaceDiffTarget::BranchCommitted { .. }, WorkspaceDiffFileSide::New) => {
            git_file_or_empty(workspace, "HEAD", path, limit).await?
        }
        (WorkspaceDiffTarget::All { merge_base_branch }, WorkspaceDiffFileSide::Old) => {
            git_file_or_empty(workspace, merge_base_branch, path, limit).await?
        }
        (WorkspaceDiffTarget::Commit { sha }, WorkspaceDiffFileSide::New) => {
            git_file_or_empty(workspace, sha, path, limit).await?
        }
        (WorkspaceDiffTarget::Commit { sha }, WorkspaceDiffFileSide::Old) => {
            let parent = git_optional(
                workspace,
                vec!["rev-parse".into(), format!("{sha}^")],
                DEFAULT_COMMAND_OUTPUT_BYTES,
            )
            .await?;
            match parent {
                Some(parent) => git_file_or_empty(workspace, &parent, path, limit).await?,
                None => Vec::new(),
            }
        }
    };
    if bytes.len() > limit {
        return Err(Failure::new(
            "file_too_large",
            format!("diff file {path:?} exceeds the requested byte limit"),
        ));
    }
    let (content, encoding) = match std::str::from_utf8(&bytes) {
        Ok(text) => (text.to_owned(), "utf8"),
        Err(_) => (
            base64::engine::general_purpose::STANDARD.encode(&bytes),
            "base64",
        ),
    };
    Ok(json!({
        "path": path,
        "content": content,
        "contentEncoding": encoding,
        "sizeBytes": bytes.len(),
        "mimeType": mime_type(path),
    }))
}

async fn git_file_or_empty(
    workspace: &Path,
    revision: &str,
    path: &str,
    limit: usize,
) -> Result<Vec<u8>, Failure> {
    let object = format!("{revision}:{path}");
    let size = match run_git(
        workspace,
        vec!["cat-file".into(), "-s".into(), object.clone()],
        128,
    )
    .await
    {
        Ok(output) => {
            ensure_complete(&output, "git blob metadata")?;
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<u64>()
                .map_err(|error| {
                    Failure::new(
                        "unknown",
                        format!("git returned an invalid blob size: {error}"),
                    )
                })?
        }
        Err(error) if error.code == "path_not_found" || error.code == "unknown" => {
            return Ok(Vec::new())
        }
        Err(error) => return Err(error),
    };
    if size > limit as u64 {
        return Err(Failure::new(
            "file_too_large",
            format!("diff file {path:?} exceeds the requested byte limit"),
        ));
    }
    let output = run_git(
        workspace,
        vec!["show".into(), object],
        limit.saturating_add(1),
    )
    .await;
    match output {
        Ok(output) if output.stdout_truncated || output.stdout.len() > limit => Err(Failure::new(
            "file_too_large",
            format!("diff file {path:?} exceeds the requested byte limit"),
        )),
        Ok(output) if output.stderr_truncated => Err(Failure::new(
            "output_truncated",
            "git file output exceeded the worker output limit",
        )),
        Ok(output) => Ok(output.stdout),
        Err(error) if error.code == "path_not_found" || error.code == "unknown" => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

async fn read_optional_file(path: &Path, limit: usize) -> Result<Vec<u8>, Failure> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_file() => {
            if metadata.len() > limit as u64 {
                return Err(Failure::new(
                    "file_too_large",
                    format!(
                        "diff file {} exceeds the requested byte limit",
                        path.display()
                    ),
                ));
            }
            let file = tokio::fs::File::open(path).await.map_err(|error| {
                Failure::new("permission_denied", format!("{}: {error}", path.display()))
            })?;
            let mut bytes = Vec::with_capacity(metadata.len().min(limit as u64) as usize);
            file.take(limit as u64 + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|error| {
                    Failure::new("permission_denied", format!("{}: {error}", path.display()))
                })?;
            if bytes.len() > limit {
                return Err(Failure::new(
                    "file_too_large",
                    format!(
                        "diff file {} exceeds the requested byte limit",
                        path.display()
                    ),
                ));
            }
            Ok(bytes)
        }
        Ok(_) => Ok(Vec::new()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(Failure::new(
            "permission_denied",
            format!("{}: {error}", path.display()),
        )),
    }
}

/// Resolves `raw` inside `workspace`, refusing anything that leaves it.
///
/// `pub(crate)` because automation scripts run a file the control plane named
/// the same way a host file request reads one: the check has to be the
/// filesystem one, and there is exactly one implementation of it.
pub(crate) async fn safe_workspace_path(workspace: &Path, raw: &str) -> Result<PathBuf, Failure> {
    validate_relative_path(raw)?;
    let candidate = workspace.join(raw);
    let canonical = match tokio::fs::canonicalize(&candidate).await {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = candidate.parent().ok_or_else(|| {
                Failure::new("path_not_found", format!("invalid workspace path {raw:?}"))
            })?;
            let parent = tokio::fs::canonicalize(parent)
                .await
                .map_err(|error| Failure::new("path_not_found", format!("{raw:?}: {error}")))?;
            parent.join(candidate.file_name().ok_or_else(|| {
                Failure::new("path_not_found", format!("invalid workspace path {raw:?}"))
            })?)
        }
        Err(error) => {
            return Err(Failure::new(
                "permission_denied",
                format!("{raw:?}: {error}"),
            ))
        }
    };
    if !canonical.starts_with(workspace) {
        return Err(Failure::new(
            "invalid_path",
            format!("path {raw:?} leaves the workspace boundary"),
        ));
    }
    Ok(canonical)
}

fn validate_relative_path(raw: &str) -> Result<(), Failure> {
    if raw.is_empty()
        || raw.contains('\0')
        || raw.contains('\\')
        || Path::new(raw).is_absolute()
        || raw
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
        || Path::new(raw)
            .components()
            .any(|component| matches!(component, Component::RootDir | Component::ParentDir))
    {
        return Err(Failure::new(
            "invalid_path",
            format!("invalid relative path {raw:?}"),
        ));
    }
    Ok(())
}

async fn workspace_commit(workspace: &Path, message: &str) -> Result<Value, Failure> {
    let message = message.trim();
    if message.is_empty() {
        return Err(Failure::new("unknown", "commit message must not be empty"));
    }
    run_git(
        workspace,
        vec!["add".into(), "--all".into(), "--".into()],
        DEFAULT_COMMAND_OUTPUT_BYTES,
    )
    .await?;
    run_git(
        workspace,
        vec!["commit".into(), "-m".into(), message.into()],
        DEFAULT_COMMAND_OUTPUT_BYTES,
    )
    .await?;
    let commit = git_optional(
        workspace,
        vec!["log".into(), "-1".into(), "--format=%H%x09%s".into()],
        DEFAULT_COMMAND_OUTPUT_BYTES,
    )
    .await?
    .ok_or_else(|| Failure::new("unknown", "git did not report the new commit"))?;
    let (sha, subject) = commit
        .split_once('\t')
        .unwrap_or((commit.as_str(), message));
    Ok(json!({ "commitSha": sha, "commitSubject": subject }))
}

async fn git_optional(
    workspace: &Path,
    args: Vec<String>,
    max_output_bytes: usize,
) -> Result<Option<String>, Failure> {
    match run_git(workspace, args, max_output_bytes).await {
        Ok(output) => {
            ensure_complete(&output, "git metadata")?;
            Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            ))
        }
        Err(error) if error.code == "not_git_repo" || error.code == "unknown" => Ok(None),
        Err(error) => Err(error),
    }
}

fn ensure_complete(output: &CommandOutput, operation: &str) -> Result<(), Failure> {
    if output.stdout_truncated || output.stderr_truncated {
        return Err(Failure::new(
            "output_truncated",
            format!("{operation} exceeded the worker output limit"),
        ));
    }
    Ok(())
}

async fn run_git(
    workspace: &Path,
    args: Vec<String>,
    max_output_bytes: usize,
) -> Result<CommandOutput, Failure> {
    let limit = max_output_bytes.clamp(1, MAX_COMMAND_OUTPUT_BYTES);
    let mut command = Command::new("git");
    command
        .args(&args)
        .current_dir(workspace)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| {
        Failure::new(
            "permission_denied",
            format!("could not start git in {}: {error}", workspace.display()),
        )
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Failure::new("unknown", "git stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Failure::new("unknown", "git stderr was not piped"))?;
    let stdout_task = tokio::spawn(read_limited(stdout, limit));
    let stderr_task = tokio::spawn(read_limited(stderr, limit));
    let status = match tokio::time::timeout(GIT_COMMAND_TIMEOUT, child.wait()).await {
        Ok(result) => result.map_err(|error| Failure::new("unknown", error.to_string()))?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(Failure::new(
                "unknown",
                format!(
                    "git command timed out after {} seconds",
                    GIT_COMMAND_TIMEOUT.as_secs()
                ),
            ));
        }
    };
    let stdout = stdout_task
        .await
        .map_err(|error| Failure::new("unknown", error.to_string()))?;
    let stderr = stderr_task
        .await
        .map_err(|error| Failure::new("unknown", error.to_string()))?;
    let output = CommandOutput {
        stdout: stdout.0,
        stderr: stderr.0,
        stdout_truncated: stdout.1,
        stderr_truncated: stderr.1,
    };
    if !status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let diagnostic = if stderr.is_empty() {
            stdout.clone()
        } else if stdout.is_empty() {
            stderr.clone()
        } else {
            format!("{stderr}\n{stdout}")
        };
        let code = if diagnostic.contains("not a git repository")
            || diagnostic.contains("not a git repo")
        {
            "not_git_repo"
        } else if args.iter().any(|arg| arg == "commit")
            && (diagnostic.contains("nothing to commit")
                || diagnostic.contains("no changes added to commit"))
        {
            "no_changes"
        } else if args.iter().any(|arg| arg == "show") {
            "path_not_found"
        } else {
            "unknown"
        };
        return Err(Failure::new(
            code,
            if diagnostic.is_empty() {
                format!("git command failed with status {status}")
            } else {
                diagnostic
            },
        ));
    }
    Ok(output)
}

async fn read_limited<R: AsyncRead + Unpin>(mut reader: R, limit: usize) -> (Vec<u8>, bool) {
    let mut output = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0u8; 16 * 1024];
    let mut truncated = false;
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        if output.len() < limit {
            let keep = (limit - output.len()).min(read);
            output.extend_from_slice(&buffer[..keep]);
            if keep < read {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    (output, truncated)
}

fn bounded_bytes(raw: u64) -> usize {
    if raw == 0 {
        DEFAULT_COMMAND_OUTPUT_BYTES
    } else {
        usize::try_from(raw)
            .unwrap_or(MAX_COMMAND_OUTPUT_BYTES)
            .clamp(1, MAX_COMMAND_OUTPUT_BYTES)
    }
}

fn bounded_count(raw: u64, default: usize) -> usize {
    if raw == 0 {
        default
    } else {
        usize::try_from(raw).unwrap_or(1_000).clamp(1, 1_000)
    }
}

fn truncate_to_char_boundary(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let boundary = value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= max_bytes)
        .last()
        .unwrap_or(0);
    value.truncate(boundary);
}

fn mime_type(path: &str) -> Option<&'static str> {
    let extension = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
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
    })
}

/// The most prompt commands one workspace may advertise.
const MAX_LISTED_COMMANDS: usize = 500;

/// Lists the prompt commands a workspace makes available.
///
/// The discovery itself belongs to `pi-acp` — the same code the ACP adapter
/// runs when it advertises commands to a client — so loom does not maintain a
/// second, drifting notion of where a slash command lives. The worker answers
/// plain rows; the control plane projects them into bb's contract shape.
///
/// The project directories are scanned first so a project's command shadows a
/// user command of the same name, which is what a per-repository prompt file is
/// for.
async fn list_commands(cwd: &str) -> Result<Value, Failure> {
    let cwd = PathBuf::from(cwd);
    let commands = tokio::task::spawn_blocking(move || {
        let file_commands = pi_acp::commands::load_slash_commands(&cwd);
        let mut names: Vec<String> = file_commands
            .iter()
            .map(|command| command.name.clone())
            .collect();
        names.extend(
            pi_acp::commands::builtin_available_commands()
                .into_iter()
                .map(|command| command.name),
        );
        names.sort();
        names.dedup();
        names.truncate(MAX_LISTED_COMMANDS);
        let rows = names
            .into_iter()
            .map(|name| {
                let file = file_commands.iter().find(|command| command.name == name);
                json!({
                    "name": name,
                    // A project prompt is `origin: project`; a built-in is the
                    // agent's own, which the contract calls `builtin`. loom has
                    // no user-level prompt directory of its own, so nothing is
                    // ever reported as `user`.
                    "origin": if file.is_some() { "project" } else { "builtin" },
                    "description": file.map(|command| command.description.clone()),
                    "argumentHint": Value::Null,
                })
            })
            .collect::<Vec<_>>();
        rows
    })
    .await
    .map_err(|error| {
        Failure::new(
            "internal_error",
            format!("command listing panicked: {error}"),
        )
    })?;
    Ok(json!({ "commands": commands }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_status_is_projected_to_contract_letters() {
        let entries = parse_status(" M tracked.rs\n?? new.txt\nR  old.rs -> new.rs\n");
        assert_eq!(entries[0].status, "M");
        assert_eq!(entries[1].status, "??");
        assert_eq!(entries[2].status, "R");
        assert_eq!(entries[2].path, "new.rs");
    }

    #[test]
    fn relative_paths_reject_traversal_and_platform_separators() {
        assert!(validate_relative_path("src/main.rs").is_ok());
        assert!(validate_relative_path("../secret").is_err());
        assert!(validate_relative_path("src/../secret").is_err());
        assert!(validate_relative_path("src\\main.rs").is_err());
        assert!(validate_relative_path("/etc/passwd").is_err());
    }

    #[test]
    fn status_projection_uses_the_actual_change_kind() {
        let entry = DiffEntry {
            path: "new.rs".into(),
            previous_path: Some("old.rs".into()),
            status: "R".into(),
            additions: Some(2),
            deletions: Some(1),
            binary: false,
            origin: "tracked",
        };
        let value = diff_file_value(&entry);
        assert_eq!(value["changeKind"], "renamed");
        assert_eq!(value["previousPath"], "old.rs");
    }
}

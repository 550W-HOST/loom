//! Real Git coverage for the daemon half of the B6 workspace contract.

use std::path::Path;
use std::process::{Command, Output};

use loom_daemon::workspace::answer;
use loom_domain::{EnvironmentId, HostId};
use loom_provider_protocol::{
    HostRpcOperation, HostRpcOutcome, HostRpcReport, HostRpcRequest, WorkspaceContext,
    WorkspaceDiffFileSide, WorkspaceDiffTarget,
};
use serde_json::Value;
use tempfile::{tempdir, TempDir};

fn git(path: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap_or_else(|error| panic!("failed to start git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn repository() -> TempDir {
    let directory = tempdir().unwrap();
    git(directory.path(), &["init", "-q"]);
    git(directory.path(), &["config", "user.name", "B6 Test"]);
    git(
        directory.path(),
        &["config", "user.email", "b6@example.invalid"],
    );
    git(directory.path(), &["checkout", "-q", "-b", "main"]);
    std::fs::write(directory.path().join("README.md"), "before\n").unwrap();
    git(directory.path(), &["add", "--", "README.md"]);
    git(directory.path(), &["commit", "-qm", "initial"]);
    directory
}

fn request(operation: HostRpcOperation) -> HostRpcRequest {
    HostRpcRequest {
        request_id: "b6-test-request".into(),
        host_id: HostId::mint(),
        operation,
        created_at_ms: 1,
    }
}

fn context(directory: &TempDir) -> WorkspaceContext {
    WorkspaceContext {
        workspace_path: directory.path().to_string_lossy().into_owned(),
    }
}

fn result(report: HostRpcReport) -> Value {
    match report.outcome {
        HostRpcOutcome::Result { result } => result,
        HostRpcOutcome::Failed { code, message } => {
            panic!("workspace request failed with {code}: {message}")
        }
    }
}

fn failure(report: HostRpcReport) -> (String, String) {
    match report.outcome {
        HostRpcOutcome::Failed { code, message } => (code, message),
        HostRpcOutcome::Result { result } => panic!("expected failure, got {result}"),
    }
}

#[tokio::test]
async fn status_diff_and_patch_are_read_from_a_real_git_workspace() {
    let directory = repository();
    std::fs::write(directory.path().join("README.md"), "after\n").unwrap();
    std::fs::write(directory.path().join("new.txt"), "untracked\n").unwrap();
    let workspace = context(&directory);

    let status = result(
        answer(request(HostRpcOperation::WorkspaceStatus {
            environment_id: EnvironmentId::mint(),
            workspace_context: workspace.clone(),
            merge_base_branch: Some("main".into()),
            max_untracked_line_stat_files: 10,
            max_untracked_line_stat_bytes: 1_024,
        }))
        .await,
    );
    assert_eq!(status["outcome"], "available");
    assert_eq!(
        status["workspace"]["workingTree"]["hasUncommittedChanges"],
        true
    );
    assert!(status["workspace"]["workingTree"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|file| file["path"] == "README.md"));

    let diff = result(
        answer(request(HostRpcOperation::WorkspaceDiff {
            environment_id: EnvironmentId::mint(),
            workspace_context: workspace.clone(),
            target: WorkspaceDiffTarget::Uncommitted,
            max_diff_bytes: 4_096,
            max_file_list_bytes: 4_096,
            max_untracked_files: 10,
        }))
        .await,
    );
    assert_eq!(diff["outcome"], "available");
    assert!(diff["diff"]["diff"].as_str().unwrap().contains("-before"));
    assert!(diff["diff"]["files"]
        .as_str()
        .unwrap()
        .contains("README.md"));

    let patch = result(
        answer(request(HostRpcOperation::WorkspaceDiffPatch {
            environment_id: EnvironmentId::mint(),
            workspace_context: workspace,
            target: WorkspaceDiffTarget::Uncommitted,
            paths: vec!["README.md".into()],
            max_bytes_per_file: 4_096,
        }))
        .await,
    );
    assert!(patch["patches"][0]["patch"]
        .as_str()
        .unwrap()
        .contains("-before"));
}

#[tokio::test]
async fn clean_status_does_not_count_a_resolved_merge_base_and_diff_reports_untracked_truncation() {
    let directory = repository();
    let clean = result(
        answer(request(HostRpcOperation::WorkspaceStatus {
            environment_id: EnvironmentId::mint(),
            workspace_context: context(&directory),
            merge_base_branch: Some("main".into()),
            max_untracked_line_stat_files: 10,
            max_untracked_line_stat_bytes: 1_024,
        }))
        .await,
    );
    assert_eq!(clean["workspace"]["workingTree"]["state"], "clean");
    assert_eq!(
        clean["workspace"]["mergeBase"]["hasCommittedUnmergedChanges"],
        false
    );

    std::fs::write(directory.path().join("a.txt"), "a\n").unwrap();
    std::fs::write(directory.path().join("b.txt"), "b\n").unwrap();
    let diff = result(
        answer(request(HostRpcOperation::WorkspaceDiff {
            environment_id: EnvironmentId::mint(),
            workspace_context: context(&directory),
            target: WorkspaceDiffTarget::Uncommitted,
            max_diff_bytes: 4_096,
            max_file_list_bytes: 4_096,
            max_untracked_files: 1,
        }))
        .await,
    );
    assert_eq!(diff["diff"]["truncated"], true);
    assert_eq!(
        diff["diff"]["files"]
            .as_str()
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("??\t"))
            .count(),
        1
    );
}

#[tokio::test]
async fn diff_file_checks_limits_and_rejects_paths_outside_the_workspace() {
    let directory = repository();
    std::fs::write(directory.path().join("large.txt"), "12345").unwrap();
    let workspace = context(&directory);

    let (code, _) = failure(
        answer(request(HostRpcOperation::WorkspaceDiffFile {
            environment_id: EnvironmentId::mint(),
            workspace_context: workspace.clone(),
            target: WorkspaceDiffTarget::Uncommitted,
            path: "large.txt".into(),
            side: WorkspaceDiffFileSide::New,
            max_bytes: 2,
        }))
        .await,
    );
    assert_eq!(code, "file_too_large");

    let (code, message) = failure(
        answer(request(HostRpcOperation::WorkspaceDiffPatch {
            environment_id: EnvironmentId::mint(),
            workspace_context: workspace,
            target: WorkspaceDiffTarget::Uncommitted,
            paths: vec!["../outside".into()],
            max_bytes_per_file: 4_096,
        }))
        .await,
    );
    assert_eq!(code, "invalid_path");
    assert!(message.contains("relative path"));
}

#[tokio::test]
async fn non_git_workspaces_and_unavailable_pr_capabilities_are_explicit() {
    let directory = tempdir().unwrap();
    let workspace = context(&directory);
    let (code, _) = failure(
        answer(request(HostRpcOperation::WorkspaceStatus {
            environment_id: EnvironmentId::mint(),
            workspace_context: workspace.clone(),
            merge_base_branch: None,
            max_untracked_line_stat_files: 10,
            max_untracked_line_stat_bytes: 1_024,
        }))
        .await,
    );
    assert_eq!(code, "not_git_repo");

    let (code, message) = failure(
        answer(request(HostRpcOperation::WorkspacePullRequest {
            environment_id: EnvironmentId::mint(),
            workspace_context: workspace,
        }))
        .await,
    );
    assert_eq!(code, "pull_request_unavailable");
    assert!(message.contains("unavailable"));
}

#[tokio::test]
async fn committing_a_clean_workspace_returns_no_changes() {
    let directory = repository();
    let (code, _) = failure(
        answer(request(HostRpcOperation::WorkspaceCommit {
            environment_id: EnvironmentId::mint(),
            workspace_context: context(&directory),
            message: "Commit workspace changes".into(),
        }))
        .await,
    );
    assert_eq!(code, "no_changes");
}

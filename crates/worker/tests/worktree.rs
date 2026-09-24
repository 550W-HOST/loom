//! Real Git coverage for managed worktree provisioning.
//!
//! These exercise [`loom_worker::worktree::provision`] and `remove` directly:
//! no server, no relay, just a real repository and the worker's git calls.
//! The server-side lifecycle is covered by the provisioning e2e tests.

use std::path::Path;
use std::process::{Command, Output};

use loom_domain::EnvironmentId;
use loom_provider_protocol::EnvironmentProvisionOutcome;
use loom_worker::worktree;
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

fn git_text(path: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&git(path, args).stdout)
        .trim()
        .to_owned()
}

fn repository() -> TempDir {
    let directory = tempdir().unwrap();
    git(directory.path(), &["init", "-q"]);
    git(directory.path(), &["config", "user.name", "Worktree Test"]);
    git(
        directory.path(),
        &["config", "user.email", "worktree@example.invalid"],
    );
    git(directory.path(), &["checkout", "-q", "-b", "main"]);
    std::fs::write(directory.path().join("README.md"), "before\n").unwrap();
    git(directory.path(), &["add", "--", "README.md"]);
    git(directory.path(), &["commit", "-qm", "initial"]);
    directory
}

fn provisioned(outcome: EnvironmentProvisionOutcome) -> (String, String, String, String) {
    match outcome {
        EnvironmentProvisionOutcome::Provisioned {
            path,
            branch_name,
            base_branch,
            default_branch,
            is_git_repo,
        } => {
            assert_eq!(is_git_repo, Some(true), "a worktree is a git repository");
            (
                path,
                branch_name.expect("a worktree reports its branch"),
                base_branch.expect("a worktree reports the base it used"),
                default_branch.expect("a repository with a branch reports a default"),
            )
        }
        EnvironmentProvisionOutcome::Failed { error } => panic!("expected a worktree: {error}"),
    }
}

fn failed(outcome: EnvironmentProvisionOutcome) -> String {
    match outcome {
        EnvironmentProvisionOutcome::Failed { error } => error,
        EnvironmentProvisionOutcome::Provisioned { path, .. } => {
            panic!("expected a failure, got a worktree at {path}")
        }
    }
}

#[tokio::test]
async fn provisioning_cuts_a_worktree_and_reports_branch_and_base() {
    let source = repository();
    let root = tempdir().unwrap();
    let environment_id = EnvironmentId::mint();

    let (path, branch, base, default_branch) = provisioned(
        worktree::provision(
            root.path(),
            &environment_id,
            source.path().to_str().unwrap(),
            "loom/env_test",
            None,
        )
        .await,
    );

    assert_eq!(
        path,
        root.path()
            .join(environment_id.to_string())
            .display()
            .to_string()
    );
    assert_eq!(branch, "loom/env_test");
    assert_eq!(base, "main");
    assert_eq!(default_branch, "main");
    assert!(Path::new(&path).join("README.md").is_file());
    assert_eq!(
        git_text(Path::new(&path), &["rev-parse", "--abbrev-ref", "HEAD"]),
        "loom/env_test"
    );
    let marker = format!("{path}.loom-completed");
    let contents = std::fs::read_to_string(&marker).unwrap();
    assert!(contents.starts_with("loom/env_test\n"), "{contents:?}");
    // The worktree is registered with the source repository.
    assert!(git_text(source.path(), &["worktree", "list"]).contains(&path));
}

#[tokio::test]
async fn a_redelivered_provision_adopts_the_existing_worktree() {
    let source = repository();
    let root = tempdir().unwrap();
    let environment_id = EnvironmentId::mint();
    let source_path = source.path().to_str().unwrap().to_owned();

    let first = provisioned(
        worktree::provision(
            root.path(),
            &environment_id,
            &source_path,
            "loom/env_test",
            None,
        )
        .await,
    );
    let second = provisioned(
        worktree::provision(
            root.path(),
            &environment_id,
            &source_path,
            "loom/env_test",
            None,
        )
        .await,
    );
    assert_eq!(first.0, second.0);
}

#[tokio::test]
async fn provisioning_repairs_a_missing_completion_marker() {
    let source = repository();
    let root = tempdir().unwrap();
    let environment_id = EnvironmentId::mint();
    let source_path = source.path().to_str().unwrap().to_owned();

    let (path, ..) = provisioned(
        worktree::provision(
            root.path(),
            &environment_id,
            &source_path,
            "loom/env_test",
            None,
        )
        .await,
    );
    // Simulate a crash between `worktree add` and the include copy finishing.
    std::fs::remove_file(format!("{path}.loom-completed")).unwrap();
    // The ignored file has to reappear when the retry re-runs the copy.
    std::fs::write(source.path().join(".gitignore"), "*.env\n").unwrap();
    std::fs::write(source.path().join(".worktreeinclude"), "*.env\n").unwrap();
    git(
        source.path(),
        &["add", "--", ".gitignore", ".worktreeinclude"],
    );
    git(source.path(), &["commit", "-qm", "include"]);
    std::fs::write(source.path().join("local.env"), "secret\n").unwrap();

    let (repaired, ..) = provisioned(
        worktree::provision(
            root.path(),
            &environment_id,
            &source_path,
            "loom/env_test",
            None,
        )
        .await,
    );
    assert_eq!(repaired, path);
    assert_eq!(
        std::fs::read_to_string(Path::new(&repaired).join("local.env")).unwrap(),
        "secret\n"
    );
}

#[tokio::test]
async fn worktree_include_copies_ignored_files_and_skips_present_ones() {
    let source = repository();
    std::fs::write(source.path().join(".gitignore"), "*.env\n").unwrap();
    std::fs::write(source.path().join(".worktreeinclude"), "*.env\n").unwrap();
    git(
        source.path(),
        &["add", "--", ".gitignore", ".worktreeinclude"],
    );
    git(source.path(), &["commit", "-qm", "ignore rules"]);
    std::fs::write(source.path().join("local.env"), "secret\n").unwrap();
    std::fs::write(source.path().join("tracked.txt"), "tracked\n").unwrap();
    git(source.path(), &["add", "--", "tracked.txt"]);
    git(source.path(), &["commit", "-qm", "tracked file"]);

    let root = tempdir().unwrap();
    let (path, ..) = provisioned(
        worktree::provision(
            root.path(),
            &EnvironmentId::mint(),
            source.path().to_str().unwrap(),
            "loom/env_include",
            None,
        )
        .await,
    );

    assert_eq!(
        std::fs::read_to_string(Path::new(&path).join("local.env")).unwrap(),
        "secret\n"
    );
    // Tracked files come from git, not the include copy.
    assert_eq!(
        std::fs::read_to_string(Path::new(&path).join("tracked.txt")).unwrap(),
        "tracked\n"
    );
}

#[tokio::test]
async fn a_named_base_branch_is_used() {
    let source = repository();
    git(source.path(), &["checkout", "-q", "-b", "release"]);
    std::fs::write(source.path().join("README.md"), "release\n").unwrap();
    git(source.path(), &["add", "--", "README.md"]);
    git(source.path(), &["commit", "-qm", "release content"]);
    git(source.path(), &["checkout", "-q", "main"]);

    let root = tempdir().unwrap();
    let (path, _, base, _) = provisioned(
        worktree::provision(
            root.path(),
            &EnvironmentId::mint(),
            source.path().to_str().unwrap(),
            "loom/env_release",
            Some("release"),
        )
        .await,
    );
    assert_eq!(base, "release");
    assert_eq!(
        std::fs::read_to_string(Path::new(&path).join("README.md")).unwrap(),
        "release\n"
    );
}

#[tokio::test]
async fn an_unknown_base_branch_is_refused_by_name() {
    let source = repository();
    let root = tempdir().unwrap();
    let error = failed(
        worktree::provision(
            root.path(),
            &EnvironmentId::mint(),
            source.path().to_str().unwrap(),
            "loom/env_missing",
            Some("does-not-exist"),
        )
        .await,
    );
    assert!(error.contains("does-not-exist"), "{error}");
}

#[tokio::test]
async fn a_plain_directory_at_the_target_is_refused() {
    let source = repository();
    let root = tempdir().unwrap();
    let environment_id = EnvironmentId::mint();
    let target = root.path().join(environment_id.to_string());
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("keep.txt"), "mine\n").unwrap();

    let error = failed(
        worktree::provision(
            root.path(),
            &environment_id,
            source.path().to_str().unwrap(),
            "loom/env_test",
            None,
        )
        .await,
    );
    assert!(error.contains("already exists"), "{error}");
    // The refusal must not have deleted what was there.
    assert_eq!(
        std::fs::read_to_string(target.join("keep.txt")).unwrap(),
        "mine\n"
    );
}

#[tokio::test]
async fn an_unborn_repository_is_refused() {
    let source = tempdir().unwrap();
    git(source.path(), &["init", "-q", "-b", "main"]);
    let root = tempdir().unwrap();
    let error = failed(
        worktree::provision(
            root.path(),
            &EnvironmentId::mint(),
            source.path().to_str().unwrap(),
            "loom/env_unborn",
            None,
        )
        .await,
    );
    assert!(error.contains("no commits"), "{error}");
}

#[tokio::test]
async fn a_source_that_is_not_a_checkout_is_refused() {
    let source = tempdir().unwrap();
    let root = tempdir().unwrap();
    let error = failed(
        worktree::provision(
            root.path(),
            &EnvironmentId::mint(),
            source.path().to_str().unwrap(),
            "loom/env_nogit",
            None,
        )
        .await,
    );
    assert!(error.contains("not a git checkout"), "{error}");
}

#[tokio::test]
async fn removal_unregisters_the_worktree_and_is_idempotent() {
    let source = repository();
    let root = tempdir().unwrap();
    let (path, ..) = provisioned(
        worktree::provision(
            root.path(),
            &EnvironmentId::mint(),
            source.path().to_str().unwrap(),
            "loom/env_remove",
            None,
        )
        .await,
    );
    worktree::remove(Path::new(&path)).await.unwrap();
    assert!(!Path::new(&path).exists());
    assert!(!Path::new(&format!("{path}.loom-completed")).exists());
    assert!(!git_text(source.path(), &["worktree", "list"]).contains(&path));

    // A redelivered teardown is not an error.
    worktree::remove(Path::new(&path)).await.unwrap();
}

#[tokio::test]
async fn removal_removes_a_managed_directory_too() {
    let root = tempdir().unwrap();
    let path = root.path().join("env_plain");
    std::fs::create_dir_all(path.join("nested")).unwrap();
    std::fs::write(path.join("nested/file.txt"), "x\n").unwrap();

    worktree::remove(&path).await.unwrap();
    assert!(!path.exists());
}

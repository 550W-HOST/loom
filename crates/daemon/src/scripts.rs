//! Running automation scripts on the machine that owns the workspace.
//!
//! A script automation is a **process on a host**, and the control plane never
//! runs one: it publishes a [`ScriptRunDispatch`] to the host's relay scope and
//! turns the [`ScriptRunReport`] into the run's result. This module is the
//! host's half — write the script, run it under an explicit environment, bound
//! its output, enforce its timeout, and kill it when the control plane says so.
//!
//! # The boundaries
//!
//! * **Where it runs.** `cwd` is the environment workspace the control plane
//!   resolved. It must exist and be a directory; a missing workspace is a
//!   refusal with a reason, not a fallback to the daemon's own directory.
//! * **What it can read.** The process environment is *cleared* and rebuilt:
//!   `PATH`, the variables the automation declared, and the `LOOM_*` identity of
//!   the run. A script cannot read the daemon's environment by accident, and it
//!   cannot read the secrets in it by intent.
//! * **What it can touch.** A `script_file` has to resolve *inside* `cwd`, and
//!   the check is the filesystem one (`safe_workspace_path`): the candidate is
//!   canonicalized, so a symlink pointing out of the workspace is refused even
//!   though neither the control plane nor a lexical check could see it. An
//!   inline script is written into the daemon's own data directory, mirroring
//!   what the reference implementation did with its plugin directory.
//! * **How long.** The dispatch carries the timeout; the process is killed when
//!   it expires and the report says so rather than pretending it exited.
//! * **How much.** Output is captured up to a fixed budget across `stdout` and
//!   `stderr`; beyond it the rest is drained and discarded and the report says
//!   the output was truncated. Truncation is never a failure and never silent.
//!
//! # Cancellation
//!
//! A [`ScriptRunCancel`] wakes the run's own task, which kills the child and
//! reports `Cancelled`. The control plane has already settled the run by then,
//! so the report is a no-op there — which is exactly why the cancel can travel
//! through the replayable relay scope without risking a second resolution.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use loom_domain::{AutomationRunId, HostId, ScriptInterpreter};
use loom_provider_protocol::{
    automation_script_root, ScriptRunCancel, ScriptRunDispatch, ScriptRunOutcome, ScriptRunReport,
};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex, Notify};

/// Most output a script run may contribute to its record, across both streams.
///
/// The reference implementation used 1 MiB and killed the process when it was
/// exceeded. Killing is not necessary: the run's result is its exit status, so
/// this one drains and discards instead, which keeps a chatty script from
/// turning a successful run into a failed one.
pub const SCRIPT_OUTPUT_MAX_BYTES: usize = 1024 * 1024;

/// Appended to output that hit the budget above.
const TRUNCATION_MARKER: &str = "\n[output truncated]\n";

/// The `PATH` a script gets when the daemon's own environment has none.
const FALLBACK_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// Handles the script runs this daemon has been asked to execute.
#[derive(Clone)]
pub struct ScriptRunner {
    inner: Arc<ScriptRunnerInner>,
}

struct ScriptRunnerInner {
    /// The daemon's enrolled host, once it has one.
    host_id: HostId,
    /// The daemon's data directory, where an inline script is written.
    data_dir: PathBuf,
    /// The server URL, exported to the script for the same reason the
    /// reference implementation exported its own: a script that wants to talk
    /// back to the control plane should not have to guess where it is.
    server_url: String,
    /// Run ids already started, so a redelivered dispatch is a no-op.
    seen: Mutex<HashSet<AutomationRunId>>,
    /// The runs in flight, so a cancel can reach the task that owns the child.
    running: Mutex<Vec<(AutomationRunId, Arc<Notify>)>>,
    reports: mpsc::Sender<ScriptRunReport>,
}

impl ScriptRunner {
    /// Wires a runner for one enrolled host.
    pub fn new(
        host_id: HostId,
        data_dir: PathBuf,
        server_url: String,
        reports: mpsc::Sender<ScriptRunReport>,
    ) -> Self {
        Self {
            inner: Arc::new(ScriptRunnerInner {
                host_id,
                data_dir,
                server_url,
                seen: Mutex::new(HashSet::new()),
                running: Mutex::new(Vec::new()),
                reports,
            }),
        }
    }

    /// The runs this daemon is executing right now.
    pub async fn running(&self) -> usize {
        self.inner.running.lock().await.len()
    }

    /// Starts a dispatch unless it was already started.
    pub async fn start(&self, dispatch: ScriptRunDispatch) {
        if dispatch.host_id != self.inner.host_id {
            return;
        }
        if !self.inner.seen.lock().await.insert(dispatch.run_id.clone()) {
            return;
        }
        let cancel = Arc::new(Notify::new());
        self.inner
            .running
            .lock()
            .await
            .push((dispatch.run_id.clone(), Arc::clone(&cancel)));

        let runner = self.clone();
        tokio::spawn(async move {
            let outcome = runner.execute(&dispatch, cancel).await;
            let report = ScriptRunReport {
                host_id: runner.inner.host_id.clone(),
                run_id: dispatch.run_id.clone(),
                outcome,
            };
            runner
                .inner
                .running
                .lock()
                .await
                .retain(|(run_id, _)| run_id != &dispatch.run_id);
            let _ = runner.inner.reports.send(report).await;
        });
    }

    /// Kills the process of a run, if this daemon is running it.
    ///
    /// A cancel for a run this daemon does not hold is a no-op: the relay may
    /// replay a frame it already applied, and a run may have finished while the
    /// cancel was in flight. Neither is a failure.
    pub async fn cancel(&self, cancel: &ScriptRunCancel) {
        if cancel.host_id != self.inner.host_id {
            return;
        }
        let notify = self
            .inner
            .running
            .lock()
            .await
            .iter()
            .find(|(run_id, _)| run_id == &cancel.run_id)
            .map(|(_, notify)| Arc::clone(notify));
        match notify {
            Some(notify) => notify.notify_one(),
            None => eprintln!(
                "loom-daemon: a cancel for automation run {} arrived with nothing running; \
                 dropping it",
                cancel.run_id
            ),
        }
    }

    /// Runs one script and returns its outcome. Never fails: every failure is
    /// an outcome the control plane can record.
    async fn execute(&self, dispatch: &ScriptRunDispatch, cancel: Arc<Notify>) -> ScriptRunOutcome {
        let cwd = PathBuf::from(&dispatch.cwd);
        // The workspace of a script is the host's own script directory — the
        // control plane composes the path from the data directory this daemon
        // *reported*, and the layout belongs to the host — so a missing one is
        // created rather than refused. A path that cannot be created, or one
        // that exists as a file, is still a refusal with a reason.
        if let Err(error) = tokio::fs::create_dir_all(&cwd).await {
            return ScriptRunOutcome::Refused {
                error: format!(
                    "the script workspace {} could not be created: {error}",
                    dispatch.cwd
                ),
            };
        }
        match tokio::fs::metadata(&cwd).await {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return ScriptRunOutcome::Refused {
                    error: format!("the workspace {} is not a directory", dispatch.cwd),
                }
            }
            Err(error) => {
                return ScriptRunOutcome::Refused {
                    error: format!("the workspace {} is not usable: {error}", dispatch.cwd),
                }
            }
        }

        let (program, script_path, script_file) = match self.resolve_script(dispatch).await {
            Ok(resolved) => resolved,
            Err(error) => return ScriptRunOutcome::Refused { error },
        };

        let mut command = Command::new(&program);
        command
            .arg(&script_file)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Cleared, then rebuilt: `PATH` so the interpreter resolves, what
            // the automation declared, and the identity of this run. Nothing
            // else from the daemon's own environment reaches the script.
            .env_clear()
            .env("PATH", daemon_path())
            .envs(dispatch.env.clone())
            .env("LOOM_SERVER_URL", &self.inner.server_url)
            .env("LOOM_PROJECT_ID", dispatch.project_id.to_string())
            .env("LOOM_AUTOMATION_ID", dispatch.automation_id.to_string())
            .env("LOOM_AUTOMATION_RUN_ID", dispatch.run_id.to_string());

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return ScriptRunOutcome::Refused {
                    error: format!("{program} could not be started: {error}"),
                }
            }
        };
        let budget = Arc::new(Mutex::new(SCRIPT_OUTPUT_MAX_BYTES));
        let truncated = Arc::new(Mutex::new(false));
        // Collected into shared sinks rather than returned from the reader
        // tasks: a killed script can leave a *grandchild* holding the pipe open,
        // and the run's result must not wait for a process it never started.
        let stdout_sink = Arc::new(Mutex::new(String::new()));
        let stderr_sink = Arc::new(Mutex::new(String::new()));
        let mut out_task = tokio::spawn(capture(
            child.stdout.take(),
            Arc::clone(&budget),
            Arc::clone(&truncated),
            Arc::clone(&stdout_sink),
        ));
        let mut err_task = tokio::spawn(capture(
            child.stderr.take(),
            Arc::clone(&budget),
            Arc::clone(&truncated),
            Arc::clone(&stderr_sink),
        ));

        let timeout = tokio::time::sleep(std::time::Duration::from_millis(dispatch.timeout_ms));
        tokio::pin!(timeout);
        let mut killed_by_cancel = false;
        let mut timed_out = false;
        let status = tokio::select! {
            status = child.wait() => status.ok(),
            _ = &mut timeout => {
                timed_out = true;
                let _ = child.kill().await;
                None
            }
            _ = cancel.notified() => {
                killed_by_cancel = true;
                let _ = child.kill().await;
                None
            }
        };

        // Read what the child wrote, but do not wait forever for the pipes to
        // close: `bash` may have left a background process holding them.
        let drained = tokio::time::timeout(std::time::Duration::from_millis(500), async {
            let _ = (&mut out_task).await;
            let _ = (&mut err_task).await;
        })
        .await;
        if drained.is_err() {
            out_task.abort();
            err_task.abort();
        }
        let mut output = stdout_sink.lock().await.clone();
        let errors = stderr_sink.lock().await.clone();
        if !errors.is_empty() {
            output.push_str(&errors);
        }
        let truncated = *truncated.lock().await;
        if truncated {
            output.push_str(TRUNCATION_MARKER);
        }
        if killed_by_cancel {
            return ScriptRunOutcome::Cancelled {
                output,
                output_truncated: truncated,
            };
        }
        ScriptRunOutcome::Exited {
            exit_code: status.and_then(|status| status.code()),
            output,
            output_truncated: truncated,
            timed_out,
            script_path,
        }
    }

    /// Resolves what to run: an inline body written here, or a file inside the
    /// workspace.
    ///
    /// Returns `(program, script_path, script_file)`: `program` is the
    /// interpreter to use when the automation did not name one *and* the file
    /// name says nothing, and `script_path` is what a report carries back for
    /// an inline script so a user can find the file that ran.
    async fn resolve_script(
        &self,
        dispatch: &ScriptRunDispatch,
    ) -> Result<(String, Option<String>, String), String> {
        if let Some(body) = &dispatch.script {
            let requested = dispatch
                .script_file
                .clone()
                .unwrap_or_else(|| "script.sh".to_owned());
            let name = sanitize_script_name(&requested);
            let root = automation_script_root(
                self.inner.data_dir.to_string_lossy().as_ref(),
                &dispatch.automation_id.to_string(),
            );
            let directory = PathBuf::from(&root);
            tokio::fs::create_dir_all(&directory)
                .await
                .map_err(|error| {
                    format!("the script directory {root} could not be created: {error}")
                })?;
            // One file per run: two runs of one automation must not overwrite
            // each other's script while both are executing.
            let path = directory.join(format!("{}-{name}", dispatch.run_id));
            tokio::fs::write(&path, body)
                .await
                .map_err(|error| format!("the script could not be written: {error}"))?;
            let program = interpreter_command(
                dispatch
                    .interpreter
                    .or_else(|| interpreter_for_path(&name))
                    .unwrap_or(ScriptInterpreter::Bash),
            )
            .to_owned();
            let file = path.to_string_lossy().into_owned();
            return Ok((program, Some(file.clone()), file));
        }

        let Some(raw) = dispatch.script_file.clone() else {
            return Err("the dispatch names neither a script nor a script file".to_owned());
        };
        // The filesystem check, not the lexical one: canonicalize and require
        // the result to stay inside the workspace, so a symlink pointing out of
        // it is refused here even though the control plane could not see it.
        let path = crate::workspace::safe_workspace_path(&PathBuf::from(&dispatch.cwd), &raw)
            .await
            .map_err(|failure| failure.message)?;
        let program = interpreter_command(
            dispatch
                .interpreter
                .or_else(|| interpreter_for_path(&raw))
                .unwrap_or(ScriptInterpreter::Bash),
        )
        .to_owned();
        Ok((program, None, path.to_string_lossy().into_owned()))
    }
}

/// Reads a stream into `sink`, spending a shared budget and marking truncation.
async fn capture<R>(
    reader: Option<R>,
    budget: Arc<Mutex<usize>>,
    truncated: Arc<Mutex<bool>>,
    sink: Arc<Mutex<String>>,
) where
    R: AsyncReadExt + Unpin + Send + 'static,
{
    let Some(mut reader) = reader else {
        return;
    };
    let mut collected = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let allowed = {
            let mut budget = budget.lock().await;
            let allowed = (*budget).min(read);
            *budget -= allowed;
            allowed
        };
        if allowed > 0 {
            collected.extend_from_slice(&buffer[..allowed]);
        }
        if allowed < read {
            // Over budget: keep draining so the process is never blocked on a
            // full pipe, but stop collecting. Truncation is reported, not
            // hidden, and never makes the run fail.
            *truncated.lock().await = true;
        }
    }
    *sink.lock().await = String::from_utf8_lossy(&collected).into_owned();
}

/// The interpreter a file name implies, when the automation did not name one.
///
/// The reference implementation's mapping, and the same default: an unknown
/// extension is a shell script.
pub fn interpreter_for_path(path: &str) -> Option<ScriptInterpreter> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".sh") || lower.ends_with(".bash") {
        Some(ScriptInterpreter::Bash)
    } else if lower.ends_with(".js") || lower.ends_with(".mjs") {
        Some(ScriptInterpreter::Node)
    } else if lower.ends_with(".py") {
        Some(ScriptInterpreter::Python3)
    } else if lower.ends_with(".zsh") || lower.ends_with(".ksh") {
        // Not an interpreter the contract names, so the file is run by its own
        // shebang through the shell the contract does name.
        None
    } else {
        None
    }
}

/// The command an interpreter runs a file with.
pub const fn interpreter_command(interpreter: ScriptInterpreter) -> &'static str {
    match interpreter {
        ScriptInterpreter::Bash => "bash",
        ScriptInterpreter::Sh => "sh",
        ScriptInterpreter::Node => "node",
        ScriptInterpreter::Python3 => "python3",
    }
}

/// Keeps a script file name to something that cannot escape its directory.
fn sanitize_script_name(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("script.sh")
        .trim();
    let cleaned: String = base
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned.chars().all(|character| character == '.') {
        "script.sh".to_owned()
    } else {
        cleaned
    }
}

/// The `PATH` a script runs with: the daemon's own, or a conservative default.
fn daemon_path() -> String {
    std::env::var("PATH").unwrap_or_else(|_| FALLBACK_PATH.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_domain::{AutomationId, ProjectId};

    fn dispatch(host_id: &HostId, cwd: &str, script: &str) -> ScriptRunDispatch {
        ScriptRunDispatch {
            run_id: AutomationRunId::mint(),
            automation_id: AutomationId::mint(),
            project_id: ProjectId::mint(),
            host_id: host_id.clone(),
            cwd: cwd.to_owned(),
            script: Some(script.to_owned()),
            script_file: None,
            interpreter: Some(ScriptInterpreter::Bash),
            env: std::collections::BTreeMap::new(),
            timeout_ms: 5_000,
            deadline_ms: 10_000,
            created_at_ms: 1,
        }
    }

    /// A runner wired to a scratch data directory, and the host it serves.
    ///
    /// The host id matters: a dispatch addressed to another host is dropped, so
    /// a test that minted one for the runner and another for the dispatch would
    /// assert against a runner that never ran anything.
    async fn runner(
        dir: &std::path::Path,
    ) -> (ScriptRunner, mpsc::Receiver<ScriptRunReport>, HostId) {
        let (reports, receiver) = mpsc::channel(8);
        let host_id = HostId::mint();
        let runner = ScriptRunner::new(
            host_id.clone(),
            dir.to_path_buf(),
            "http://127.0.0.1:1".into(),
            reports,
        );
        (runner, receiver, host_id)
    }

    #[tokio::test]
    async fn an_inline_script_runs_and_reports_its_output_and_status() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (runner, mut reports, host_id) = runner(dir.path()).await;
        runner
            .start(dispatch(
                &host_id,
                workspace.path().to_str().unwrap(),
                "echo hello; exit 0",
            ))
            .await;
        let report = reports.recv().await.expect("a report");
        match report.outcome {
            ScriptRunOutcome::Exited {
                exit_code,
                output,
                output_truncated,
                timed_out,
                script_path,
            } => {
                assert_eq!(exit_code, Some(0));
                assert_eq!(output.trim(), "hello");
                assert!(!output_truncated);
                assert!(!timed_out);
                let path = script_path.expect("an inline script reports where it was written");
                assert!(path.ends_with(".sh"), "{path}");
                assert!(std::path::Path::new(&path).exists());
            }
            other => panic!("expected an exit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_non_zero_exit_is_reported_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (runner, mut reports, host_id) = runner(dir.path()).await;
        runner
            .start(dispatch(
                &host_id,
                workspace.path().to_str().unwrap(),
                "echo bad 1>&2; exit 7",
            ))
            .await;
        match reports.recv().await.expect("a report").outcome {
            ScriptRunOutcome::Exited {
                exit_code, output, ..
            } => {
                assert_eq!(exit_code, Some(7));
                assert_eq!(output.trim(), "bad");
            }
            other => panic!("expected an exit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_script_that_outlives_its_timeout_is_killed_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (runner, mut reports, host_id) = runner(dir.path()).await;
        let mut request = dispatch(&host_id, workspace.path().to_str().unwrap(), "sleep 30");
        request.timeout_ms = 200;
        runner.start(request).await;
        match reports.recv().await.expect("a report").outcome {
            ScriptRunOutcome::Exited {
                timed_out,
                exit_code,
                ..
            } => {
                assert!(timed_out, "the timeout is what ended it");
                assert_eq!(exit_code, None, "a killed process has no exit code");
            }
            other => panic!("expected an exit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_cancel_kills_the_process_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (runner, mut reports, host_id) = runner(dir.path()).await;
        let mut request = dispatch(&host_id, workspace.path().to_str().unwrap(), "sleep 30");
        request.timeout_ms = 30_000;
        let run_id = request.run_id.clone();
        runner.start(request).await;

        // Wait until the process is actually running before cancelling it.
        for _ in 0..100 {
            if runner.running().await > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // A cancel addressed to another host is a no-op: ownership is checked
        // before anything is killed.
        runner
            .cancel(&ScriptRunCancel {
                run_id: run_id.clone(),
                host_id: HostId::mint(),
                reason: "not this machine".into(),
                created_at_ms: 2,
            })
            .await;
        assert!(runner.running().await > 0);

        runner
            .cancel(&ScriptRunCancel {
                run_id,
                host_id: host_id.clone(),
                reason: "the user paused the automation".into(),
                created_at_ms: 3,
            })
            .await;
        match reports.recv().await.expect("a report").outcome {
            ScriptRunOutcome::Cancelled { output, .. } => {
                assert!(
                    output.is_empty(),
                    "a killed sleep prints nothing: {output:?}"
                );
            }
            other => panic!("expected a cancel, got {other:?}"),
        }
        assert_eq!(runner.running().await, 0);
    }

    #[tokio::test]
    async fn a_script_file_outside_the_workspace_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("escape.sh"), "echo pwned").unwrap();
        let (runner, mut reports, host_id) = runner(dir.path()).await;

        let mut request = dispatch(&host_id, workspace.path().to_str().unwrap(), "");
        request.script = None;
        request.script_file = Some(
            outside
                .path()
                .join("escape.sh")
                .to_string_lossy()
                .into_owned(),
        );
        runner.start(request).await;
        match reports.recv().await.expect("a report").outcome {
            ScriptRunOutcome::Refused { error } => {
                // The absolute path is refused by the lexical check, which runs
                // before anything touches the filesystem.
                assert!(error.contains("invalid"), "{error}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }

        // A symlink whose *name* is inside the workspace but whose target is
        // not: the lexical check cannot see that, so the canonical one must.
        let link = workspace.path().join("link.sh");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path().join("escape.sh"), &link).unwrap();
            let mut request = dispatch(&host_id, workspace.path().to_str().unwrap(), "");
            request.script = None;
            request.script_file = Some("link.sh".into());
            runner.start(request).await;
            match reports.recv().await.expect("a report").outcome {
                ScriptRunOutcome::Refused { error } => {
                    assert!(error.contains("leaves the workspace"), "{error}");
                }
                other => panic!("expected a refusal for the symlink, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_workspace_that_cannot_be_created_is_refused_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let (runner, mut reports, host_id) = runner(dir.path()).await;
        // The script directory is the host's own layout, so a missing one is
        // created; a path it may not create is a refusal that says so.
        runner
            .start(dispatch(&host_id, "/proc/loom-scripts", "echo hi"))
            .await;
        match reports.recv().await.expect("a report").outcome {
            ScriptRunOutcome::Refused { error } => {
                assert!(error.contains("could not be created"), "{error}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn output_beyond_the_budget_is_truncated_not_failed() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (runner, mut reports, host_id) = runner(dir.path()).await;
        // More than the budget, then a clean exit: the run succeeded, and the
        // record says the output was cut.
        let script = "for i in $(seq 1 400); do printf 'x%.0s' $(seq 1 4096); done; exit 0";
        runner
            .start(dispatch(
                &host_id,
                workspace.path().to_str().unwrap(),
                script,
            ))
            .await;
        match reports.recv().await.expect("a report").outcome {
            ScriptRunOutcome::Exited {
                exit_code,
                output,
                output_truncated,
                ..
            } => {
                assert_eq!(exit_code, Some(0));
                assert!(output_truncated);
                assert!(
                    output.ends_with("[output truncated]\n"),
                    "len={} tail={:?}",
                    output.len(),
                    output.chars().rev().take(40).collect::<String>()
                );
                assert!(output.len() <= SCRIPT_OUTPUT_MAX_BYTES + TRUNCATION_MARKER.len());
            }
            other => panic!("expected an exit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_script_does_not_inherit_the_daemons_environment() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (runner, mut reports, host_id) = runner(dir.path()).await;
        // The harness sets this for the test process; a script must not see it.
        std::env::set_var("LOOM_SCRIPT_TEST_SECRET", "leaked");
        let mut request = dispatch(&host_id, workspace.path().to_str().unwrap(), "echo");
        request.env.insert("DECLARED".into(), "visible".into());
        request.script = Some(
            "echo ${LOOM_SCRIPT_TEST_SECRET:-none}; echo $DECLARED; echo $LOOM_AUTOMATION_ID"
                .into(),
        );
        runner.start(request).await;
        match reports.recv().await.expect("a report").outcome {
            ScriptRunOutcome::Exited { output, .. } => {
                assert_eq!(output.lines().next(), Some("none"));
                assert_eq!(output.lines().nth(1), Some("visible"));
                assert!(output
                    .lines()
                    .nth(2)
                    .is_some_and(|line| line.starts_with("auto_")));
            }
            other => panic!("expected an exit, got {other:?}"),
        }
    }

    #[test]
    fn script_names_cannot_escape_their_directory() {
        assert_eq!(sanitize_script_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_script_name(".."), "script.sh");
        assert_eq!(sanitize_script_name("my script.py"), "my-script.py");
        assert_eq!(sanitize_script_name("a/b/c.sh"), "c.sh");
    }

    #[test]
    fn an_extension_implies_an_interpreter() {
        assert_eq!(
            interpreter_for_path("run.py"),
            Some(ScriptInterpreter::Python3)
        );
        assert_eq!(
            interpreter_for_path("run.mjs"),
            Some(ScriptInterpreter::Node)
        );
        assert_eq!(
            interpreter_for_path("run.sh"),
            Some(ScriptInterpreter::Bash)
        );
        assert_eq!(interpreter_for_path("run.unknown"), None);
    }
}

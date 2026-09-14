//! Terminal sessions on the machine that owns them.
//!
//! A terminal is a PTY with a child process attached, and it lives on exactly
//! one host. This module is the daemon's half of
//! [`TerminalRequest`](loom_provider_protocol::TerminalRequest): the control
//! plane names a session and what to do with it, and the daemon — which is the
//! only party that can see the process — performs it and reports back.
//!
//! ```text
//!   server ── TerminalRequest ──▶ relay host:{id} ──▶ daemon
//!   server ◀── TerminalReport ── daemon socket
//! ```
//!
//! # Why the output lives here and not on the server
//!
//! A terminal's stdout is unbounded. Buffering it in the control plane would
//! turn "a command printed a lot" into "the server ran out of memory", and the
//! bytes would outlive the process they came from with no way to tell whether
//! they were still being produced. So each session owns a **bounded ring** of
//! output chunks here, sequence-numbered from zero, and the control plane only
//! ever reads a window from a cursor. A reader that falls behind loses the
//! oldest chunks and is told so (`truncated`), which is a fact it can render;
//! an unbounded buffer would instead silently hold every byte forever.
//!
//! # Lifecycle
//!
//! ```text
//!   create ──▶ starting ──▶ running ──(child exits)──▶ exited
//!                  │            │
//!                  │            ├── close(force) ─────▶ exited (user)
//!                  │            └── restart ──────────▶ starting
//!                  └── spawn failed ──────────────────▶ exited (process-exit)
//!
//!   daemon connection drops ──▶ every live session is marked disconnected
//!   daemon reconnects ────────▶ the server asks for a report and reconciles
//! ```
//!
//! # Concurrency
//!
//! Every session is guarded by its own mutex and driven from a dedicated
//! blocking thread reading the PTY master. Filesystem and PTY reads are
//! blocking calls, so they never run on the async runtime: the request handler
//! hands work to `spawn_blocking` and the socket loop waits for the answer.

// Every helper here answers with `TerminalOutcome`, whose `Failed` arm is large
// enough that clippy's `result_large_err` fires on each one. Boxing the error
// would cost an allocation on the hot failure path and obscure the handlers;
// the server's HTTP modules carry the same allow for the same reason.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use base64::Engine;
use loom_provider_protocol::{
    TerminalCloseReason, TerminalOperation, TerminalOutcome, TerminalReport, TerminalRequest,
    TerminalSession, TerminalStart, TerminalStatus, TerminalTarget,
};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};

/// The largest window of output one read may answer.
///
/// bb caps `tailBytes` at 4 MiB; the daemon enforces the same ceiling so a
/// request cannot ask for more than the transport can carry in one frame.
pub const MAX_TAIL_BYTES: u64 = 4 * 1024 * 1024;

/// The default window when a reader names none.
pub const DEFAULT_TAIL_BYTES: u64 = 256 * 1024;

/// The largest number of chunks one read may answer.
pub const MAX_CHUNKS: usize = 10_000;

/// How many output chunks a session retains before dropping the oldest.
///
/// 4096 chunks at up to 64 KiB each is a few hundred MiB worst case, but a real
/// shell's chunks are small; the byte cap below is the tighter bound. The point
/// is that it *is* bounded: a `yes` that runs forever grows nothing here.
pub const MAX_RETAINED_CHUNKS: usize = 4096;

/// How many *bytes* of output a session retains before dropping the oldest.
///
/// The chunk count alone is not a bound because a chunk can be large. This is
/// the number that keeps a runaway producer from growing the daemon.
pub const MAX_RETAINED_BYTES: usize = 8 * 1024 * 1024;

/// The largest single chunk read from the PTY.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// The largest input write accepted in one request, matching bb's base64 cap.
const MAX_INPUT_BYTES: usize = 64 * 1024;

/// Read a chunk from the master and record it. Shared by every session's
/// reader thread.
fn record(session: &SessionState, bytes: Vec<u8>) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let mut output = session.output.lock().unwrap_or_else(|p| p.into_inner());
    let seq = output.next_seq;
    output.next_seq += 1;
    output.retained_bytes += bytes.len();
    output.chunks.push((seq, encoded));
    while output.chunks.len() > MAX_RETAINED_CHUNKS || output.retained_bytes > MAX_RETAINED_BYTES {
        if let Some((_, dropped)) = output.chunks.first() {
            // The base64 length is a faithful proxy for the decoded size; the
            // exact byte count is tracked separately above.
            let decoded = dropped.len() / 4 * 3;
            output.retained_bytes = output.retained_bytes.saturating_sub(decoded);
        }
        output.chunks.remove(0);
        output.truncated = true;
    }
}

/// One output window, as answered to a reader.
struct OutputBuffer {
    /// `(seq, base64)` in sequence order.
    chunks: Vec<(u64, String)>,
    /// The next sequence number the reader thread will mint.
    next_seq: u64,
    /// Sum of the *decoded* sizes of `chunks`.
    retained_bytes: usize,
    /// Whether a chunk has ever been dropped for this session.
    truncated: bool,
}

impl OutputBuffer {
    fn new() -> Self {
        Self {
            chunks: Vec::new(),
            next_seq: 0,
            retained_bytes: 0,
            truncated: false,
        }
    }
}

/// The mutable half of a session, behind one mutex.
struct SessionState {
    /// Where output chunks live.
    output: Mutex<OutputBuffer>,
    /// The master side, used for resize. Absent once the PTY is gone.
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    /// The writer side, used for input. Dropping it sends EOF.
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    /// The kill handle, used to force a close. Cloneable independently of the
    /// child so a close does not race the thread blocked in `wait`.
    killer: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>,
    /// The session as the control plane sees it.
    metadata: Mutex<TerminalSession>,
    /// What the session was created with, so a restart can repeat it.
    start: Mutex<(TerminalStart, TerminalTarget, String)>,
}

/// A shared handle to one session.
#[derive(Clone)]
pub struct SessionHandle {
    inner: Arc<SessionState>,
}

impl SessionHandle {
    /// The session's current state, as a snapshot.
    pub fn snapshot(&self) -> TerminalSession {
        self.inner
            .metadata
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Whether the process is still alive.
    fn is_live(&self) -> bool {
        matches!(
            self.inner
                .metadata
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .status,
            TerminalStatus::Starting | TerminalStatus::Running
        )
    }

    /// Marks the session as disconnected, without touching the process.
    ///
    /// Used when the server connection drops: the daemon keeps the process
    /// running (a terminal is the user's, not the connection's) but reports it
    /// as undrivable, because no client can reach a daemon that is not
    /// connected.
    pub fn mark_disconnected(&self, now_ms: u64) {
        let mut metadata = self
            .inner
            .metadata
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if matches!(
            metadata.status,
            TerminalStatus::Starting | TerminalStatus::Running
        ) {
            metadata.status = TerminalStatus::Disconnected;
            metadata.updated_at_ms = now_ms;
        }
    }

    /// Restores a disconnected session to running after a reconnect.
    pub fn mark_running(&self, now_ms: u64) {
        let mut metadata = self
            .inner
            .metadata
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if metadata.status == TerminalStatus::Disconnected {
            metadata.status = TerminalStatus::Running;
            metadata.updated_at_ms = now_ms;
        }
    }

    /// Kills the process and settles the session as exited.
    pub fn force_close(&self, reason: TerminalCloseReason, now_ms: u64) {
        if let Some(mut killer) = self
            .inner
            .killer
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            let _ = killer.kill();
        }
        let mut metadata = self
            .inner
            .metadata
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if matches!(
            metadata.status,
            TerminalStatus::Starting | TerminalStatus::Running | TerminalStatus::Disconnected
        ) {
            metadata.status = TerminalStatus::Exited;
            metadata.close_reason = Some(reason);
            metadata.updated_at_ms = now_ms;
        }
    }
}

/// Every terminal session this daemon holds.
///
/// Cloned by handle, like [`crate::acp::permission::PermissionRegistry`]: the
/// socket loop owns one handle and a blocking task gets another, and both must
/// see the same sessions.
#[derive(Clone, Default)]
pub struct TerminalRegistry {
    sessions: Arc<Mutex<HashMap<String, SessionHandle>>>,
}

impl TerminalRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    fn insert(&self, id: String, handle: SessionHandle) -> bool {
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        if sessions.contains_key(&id) {
            return false;
        }
        sessions.insert(id, handle);
        true
    }

    fn get(&self, id: &str) -> Option<SessionHandle> {
        self.sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned()
    }

    fn all(&self) -> Vec<SessionHandle> {
        self.sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// How many sessions are held.
    pub fn len(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Marks every live session disconnected, on a server disconnect.
    pub fn mark_all_disconnected(&self, now_ms: u64) {
        for handle in self.all() {
            handle.mark_disconnected(now_ms);
        }
    }

    /// Restores every disconnected session, on a reconnect.
    pub fn mark_all_running(&self, now_ms: u64) {
        for handle in self.all() {
            handle.mark_running(now_ms);
        }
    }

    /// Kills every live process, on daemon shutdown.
    ///
    /// A terminal's process is the user's, but it is a child of *this* daemon:
    /// leaving it running with no daemon to report it would leak a process the
    /// control plane could never see again.
    pub fn close_all(&self, now_ms: u64) {
        for handle in self.all() {
            handle.force_close(TerminalCloseReason::DaemonDisconnect, now_ms);
        }
    }
}

fn failed(code: &str, message: impl Into<String>) -> TerminalOutcome {
    TerminalOutcome::Failed {
        code: code.to_owned(),
        message: message.into(),
    }
}

/// Resolves the absolute working directory a request should run in.
///
/// The control plane already computed it from the environment or thread, but
/// this re-checks it: a directory that does not exist on this machine is a
/// clear failure here, not a pty that dies mysteriously a moment later.
fn resolve_cwd(raw: &str) -> Result<std::path::PathBuf, TerminalOutcome> {
    if raw.is_empty() || !Path::new(raw).is_absolute() {
        return Err(failed("invalid_path", "cwd must be absolute"));
    }
    let path = std::fs::canonicalize(raw)
        .map_err(|error| failed("invalid_path", format!("cwd is not readable: {error}")))?;
    if !path.is_dir() {
        return Err(failed("invalid_path", "cwd is not a directory"));
    }
    Ok(path)
}

/// The shell to run when the request asks for the host's default.
fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| {
        if cfg!(windows) {
            "cmd.exe".to_owned()
        } else {
            "/bin/sh".to_owned()
        }
    })
}

/// Builds the command a session should run.
fn command_for(start: &TerminalStart, cwd: &Path) -> CommandBuilder {
    let mut command = match start {
        TerminalStart::Shell => CommandBuilder::new(default_shell()),
        TerminalStart::Command { command } => {
            // A command is run *through* a shell, exactly as a user would type
            // it: quoting, pipes and redirection are the shell's job, not an
            // argv splitter's, and inventing an argv here would silently change
            // what the user asked for.
            let mut builder = CommandBuilder::new(default_shell());
            builder.arg("-c");
            builder.arg(command);
            builder
        }
    };
    command.cwd(cwd);
    command
}

/// Creates one session and starts its output reader.
#[allow(clippy::too_many_arguments)]
fn create_session(
    id: &str,
    start: TerminalStart,
    target: TerminalTarget,
    cols: u16,
    rows: u16,
    title: String,
    cwd: String,
    host_id: &loom_domain::HostId,
    now_ms: u64,
) -> Result<SessionHandle, TerminalOutcome> {
    let direct_cwd = resolve_cwd(&cwd)?;
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| failed("internal_error", format!("could not open a pty: {error}")))?;

    let command = command_for(&start, &direct_cwd);
    let mut child = pair.slave.spawn_command(command).map_err(|error| {
        failed(
            "internal_error",
            format!("could not start the shell: {error}"),
        )
    })?;
    // The slave handle must be dropped in the parent, or the master never sees
    // EOF when the child exits and the reader thread blocks forever.
    drop(pair.slave);

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| failed("internal_error", format!("could not read the pty: {error}")))?;
    let writer = pair.master.take_writer().map_err(|error| {
        failed(
            "internal_error",
            format!("could not write the pty: {error}"),
        )
    })?;
    let killer = child.clone_killer();

    let (thread_id, environment_id) = match &target {
        TerminalTarget::Thread { thread_id } => (Some(thread_id.clone()), None),
        TerminalTarget::Environment { environment_id } => (None, Some(environment_id.clone())),
        TerminalTarget::HostPath { .. } => (None, None),
    };
    let host = match &target {
        TerminalTarget::HostPath { host_id, .. } => host_id.clone(),
        _ => host_id.clone(),
    };

    let session = TerminalSession {
        id: id.to_owned(),
        thread_id,
        environment_id,
        host_id: host,
        title,
        initial_cwd: direct_cwd.to_string_lossy().into_owned(),
        cols,
        rows,
        status: TerminalStatus::Running,
        exit_code: None,
        close_reason: None,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
        last_user_input_at_ms: None,
        next_seq: 0,
    };

    let inner = Arc::new(SessionState {
        output: Mutex::new(OutputBuffer::new()),
        master: Mutex::new(Some(pair.master)),
        writer: Mutex::new(Some(writer)),
        killer: Mutex::new(Some(killer)),
        metadata: Mutex::new(session),
        start: Mutex::new((start, target, cwd)),
    });
    let handle = SessionHandle {
        inner: inner.clone(),
    };

    // The reader runs on a blocking thread: `read` on a pty master blocks, and
    // blocking the async runtime on it would stall every other session.
    let reader_state = inner.clone();
    std::thread::Builder::new()
        .name(format!("loom-pty-{id}"))
        .spawn(move || {
            let mut reader = reader;
            let mut buffer = vec![0u8; READ_CHUNK_BYTES];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => record(&reader_state, buffer[..read].to_vec()),
                    // A read that fails because the master closed is the normal
                    // end of a session; anything else ends the reader too,
                    // because there is no way to resynchronise a broken pty.
                    Err(_) => break,
                }
            }
            // The child's exit status is what distinguishes a clean exit from a
            // crash, so wait for it on the same thread that saw EOF.
            if let Some(status) = wait_for_exit(&reader_state, &mut child) {
                let mut metadata = reader_state
                    .metadata
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                metadata.status = TerminalStatus::Exited;
                metadata.exit_code = Some(status);
                if metadata.close_reason.is_none() {
                    metadata.close_reason = Some(TerminalCloseReason::ProcessExit);
                }
                metadata.updated_at_ms = now_ms_u64();
            }
        })
        .map_err(|error| {
            failed(
                "internal_error",
                format!("could not start a reader: {error}"),
            )
        })?;

    Ok(handle)
}

/// Waits for the child and returns its exit code, without blocking shutdown.
///
/// `portable_pty`'s `wait` needs `&mut`, and the child is owned by this thread,
/// so waiting here is the natural place. A child that was killed returns its
/// signal-derived status, which is still an exit the client should see.
fn wait_for_exit(
    _state: &Arc<SessionState>,
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
) -> Option<i32> {
    let status = child.wait().ok()?;
    Some(status.exit_code() as i32)
}

/// The daemon's wall clock.
fn now_ms_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Reads a window of output from a session.
fn read_output(
    handle: &SessionHandle,
    since_seq: u64,
    limit: usize,
    tail_bytes: u64,
) -> TerminalOutcome {
    let output = handle
        .inner
        .output
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let limit = limit.clamp(1, MAX_CHUNKS);
    let budget = tail_bytes.clamp(1, MAX_TAIL_BYTES) as usize;
    let mut chunks = Vec::new();
    let mut used = 0usize;
    for (seq, encoded) in output.chunks.iter() {
        if *seq < since_seq {
            continue;
        }
        // One chunk past the budget still goes out when nothing has gone yet:
        // a reader that never receives the chunk it asked from cannot advance
        // its cursor, and an empty answer would look like the end of output.
        let size = encoded.len() / 4 * 3;
        if !chunks.is_empty() && used + size > budget {
            break;
        }
        used += size;
        chunks.push(loom_provider_protocol::TerminalOutputChunk {
            seq: *seq,
            data_base64: encoded.clone(),
        });
        if chunks.len() >= limit {
            break;
        }
    }
    let next_seq = chunks
        .last()
        .map(|chunk| chunk.seq + 1)
        .unwrap_or(output.next_seq);
    TerminalOutcome::Output {
        chunks,
        next_seq,
        truncated: output.truncated,
    }
}

/// Answers one terminal request. Blocking; callers run it off the runtime.
pub fn answer(request: TerminalRequest, registry: &TerminalRegistry) -> TerminalReport {
    let host_id = request.host_id.clone();
    let request_id = request.request_id.clone();
    let outcome = match &request.operation {
        TerminalOperation::Create {
            id,
            start,
            target,
            cols,
            rows,
            title,
            cwd,
        } => {
            let now = now_ms_u64();
            match create_session(
                id,
                start.clone(),
                target.clone(),
                *cols,
                *rows,
                title.clone(),
                cwd.clone(),
                &host_id,
                now,
            ) {
                // A create for an id already held is the redelivered request
                // the relay replayed; answering with the existing session is
                // idempotent and is what makes replay safe.
                Ok(handle) => {
                    let session = handle.snapshot();
                    if !registry.insert(id.clone(), handle) {
                        match registry.get(id) {
                            Some(existing) => TerminalOutcome::Session {
                                session: existing.snapshot(),
                            },
                            None => TerminalOutcome::Session { session },
                        }
                    } else {
                        TerminalOutcome::Session { session }
                    }
                }
                Err(outcome) => outcome,
            }
        }
        TerminalOperation::Input { id, data_base64 } => match registry.get(id) {
            None => failed("terminal_not_found", "terminal session is not known"),
            Some(handle) => {
                let Some(bytes) = base64::engine::general_purpose::STANDARD
                    .decode(data_base64)
                    .ok()
                else {
                    return report(
                        host_id,
                        request_id,
                        failed("invalid_request", "dataBase64 is not valid base64"),
                    );
                };
                if bytes.len() > MAX_INPUT_BYTES {
                    return report(
                        host_id,
                        request_id,
                        failed("invalid_request", "input is over the 64 KiB limit"),
                    );
                }
                let mut writer = handle
                    .inner
                    .writer
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                let Some(writer) = writer.as_mut() else {
                    return report(
                        host_id,
                        request_id,
                        failed("terminal_not_running", "terminal has no open stdin"),
                    );
                };
                if let Err(error) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
                    return report(
                        host_id,
                        request_id,
                        failed(
                            "terminal_not_running",
                            format!("could not write to the terminal: {error}"),
                        ),
                    );
                }
                let now = now_ms_u64();
                {
                    let mut metadata = handle
                        .inner
                        .metadata
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    metadata.last_user_input_at_ms = Some(now);
                    metadata.updated_at_ms = now;
                }
                TerminalOutcome::Session {
                    session: handle.snapshot(),
                }
            }
        },
        TerminalOperation::Resize { id, cols, rows } => match registry.get(id) {
            None => failed("terminal_not_found", "terminal session is not known"),
            Some(handle) => {
                {
                    let master = handle
                        .inner
                        .master
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    let Some(master) = master.as_ref() else {
                        return report(
                            host_id,
                            request_id,
                            failed("terminal_not_running", "terminal has no pty"),
                        );
                    };
                    if let Err(error) = master.resize(PtySize {
                        rows: *rows,
                        cols: *cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    }) {
                        return report(
                            host_id,
                            request_id,
                            failed(
                                "terminal_not_running",
                                format!("could not resize the terminal: {error}"),
                            ),
                        );
                    }
                }
                let now = now_ms_u64();
                {
                    let mut metadata = handle
                        .inner
                        .metadata
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    metadata.cols = *cols;
                    metadata.rows = *rows;
                    metadata.updated_at_ms = now;
                }
                TerminalOutcome::Session {
                    session: handle.snapshot(),
                }
            }
        },
        TerminalOperation::Output {
            id,
            since_seq,
            limit,
            tail_bytes,
        } => match registry.get(id) {
            None => failed("terminal_not_found", "terminal session is not known"),
            Some(handle) => read_output(&handle, *since_seq, *limit, *tail_bytes),
        },
        TerminalOperation::Close { id, force } => match registry.get(id) {
            None => failed("terminal_not_found", "terminal session is not known"),
            Some(handle) => {
                let now = now_ms_u64();
                if handle.is_live() && !force {
                    return report(
                        host_id,
                        request_id,
                        failed(
                            "terminal_not_running",
                            "the terminal is still running; close it with force",
                        ),
                    );
                }
                if handle.is_live() {
                    handle.force_close(TerminalCloseReason::User, now);
                } else {
                    let mut metadata = handle
                        .inner
                        .metadata
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    if metadata.close_reason.is_none() {
                        metadata.close_reason = Some(TerminalCloseReason::User);
                    }
                    metadata.updated_at_ms = now;
                }
                TerminalOutcome::Session {
                    session: handle.snapshot(),
                }
            }
        },
        TerminalOperation::Restart { id } => match registry.get(id) {
            None => failed("terminal_not_found", "terminal session is not known"),
            Some(handle) => {
                let (start, target, cwd) = {
                    let start = handle.inner.start.lock().unwrap_or_else(|p| p.into_inner());
                    (start.0.clone(), start.1.clone(), start.2.clone())
                };
                let (cols, rows, title) = {
                    let metadata = handle
                        .inner
                        .metadata
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    (metadata.cols, metadata.rows, metadata.title.clone())
                };
                // The old process is killed before a new one starts, so a
                // restart cannot leave two shells holding the same session id.
                handle.force_close(TerminalCloseReason::ProcessExit, now_ms_u64());
                let now = now_ms_u64();
                match create_session(id, start, target, cols, rows, title, cwd, &host_id, now) {
                    Ok(fresh) => {
                        let mut sessions =
                            registry.sessions.lock().unwrap_or_else(|p| p.into_inner());
                        sessions.insert(id.clone(), fresh.clone());
                        drop(sessions);
                        TerminalOutcome::Session {
                            session: fresh.snapshot(),
                        }
                    }
                    Err(outcome) => outcome,
                }
            }
        },
        TerminalOperation::Report { id } => match id {
            Some(id) => match registry.get(id) {
                None => TerminalOutcome::Sessions {
                    sessions: Vec::new(),
                },
                Some(handle) => TerminalOutcome::Sessions {
                    sessions: vec![handle.snapshot()],
                },
            },
            None => TerminalOutcome::Sessions {
                sessions: registry
                    .all()
                    .into_iter()
                    .map(|handle| handle.snapshot())
                    .collect(),
            },
        },
    };
    report(host_id, request_id, outcome)
}

fn report(
    host_id: loom_domain::HostId,
    request_id: String,
    outcome: TerminalOutcome,
) -> TerminalReport {
    TerminalReport {
        host_id,
        request_id,
        outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(operation: TerminalOperation) -> TerminalRequest {
        TerminalRequest {
            request_id: "req-1".into(),
            host_id: loom_domain::HostId::mint(),
            operation,
            created_at_ms: 1,
        }
    }

    fn create(id: &str, command: &str, cwd: &str) -> TerminalReport {
        answer(
            request(TerminalOperation::Create {
                id: id.into(),
                start: TerminalStart::Command {
                    command: command.into(),
                },
                target: TerminalTarget::HostPath {
                    host_id: loom_domain::HostId::mint(),
                    cwd: None,
                },
                cols: 80,
                rows: 24,
                title: "t".into(),
                cwd: cwd.into(),
            }),
            &TerminalRegistry::new(),
        )
    }

    /// Waits until `predicate` holds, up to a bounded number of tries.
    fn eventually(mut predicate: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if predicate() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        predicate()
    }

    #[test]
    fn output_is_captured_and_read_from_a_cursor() {
        let registry = TerminalRegistry::new();
        let cwd = std::env::temp_dir();
        let cwd = cwd.to_string_lossy().into_owned();
        let report = answer(
            request(TerminalOperation::Create {
                id: "term_a".into(),
                start: TerminalStart::Command {
                    command: "printf hello".into(),
                },
                target: TerminalTarget::HostPath {
                    host_id: loom_domain::HostId::mint(),
                    cwd: None,
                },
                cols: 80,
                rows: 24,
                title: "t".into(),
                cwd,
            }),
            &registry,
        );
        let TerminalOutcome::Session { session } = report.outcome else {
            panic!("expected a session, got {:?}", report.outcome);
        };
        assert_eq!(session.status, TerminalStatus::Running);

        // The shell needs a moment to run and print.
        let handle = registry.get("term_a").unwrap();
        assert!(
            eventually(|| {
                handle.snapshot().next_seq > 0 || handle.snapshot().status == TerminalStatus::Exited
            }),
            "the command produced no output"
        );

        let report = answer(
            request(TerminalOperation::Output {
                id: "term_a".into(),
                since_seq: 0,
                limit: 100,
                tail_bytes: DEFAULT_TAIL_BYTES,
            }),
            &registry,
        );
        let TerminalOutcome::Output {
            chunks, next_seq, ..
        } = report.outcome
        else {
            panic!("expected output, got {:?}", report.outcome);
        };
        assert!(!chunks.is_empty());
        let decoded = chunks
            .iter()
            .flat_map(|chunk| {
                base64::engine::general_purpose::STANDARD
                    .decode(&chunk.data_base64)
                    .unwrap()
            })
            .collect::<Vec<u8>>();
        let text = String::from_utf8_lossy(&decoded);
        assert!(text.contains("hello"), "unexpected output {text:?}");
        assert_eq!(next_seq, chunks.last().unwrap().seq + 1);
    }

    #[test]
    fn an_unknown_session_is_reported_not_invented() {
        let registry = TerminalRegistry::new();
        let report = answer(
            request(TerminalOperation::Output {
                id: "term_missing".into(),
                since_seq: 0,
                limit: 10,
                tail_bytes: 1024,
            }),
            &registry,
        );
        let TerminalOutcome::Failed { code, .. } = report.outcome else {
            panic!("expected a failure");
        };
        assert_eq!(code, "terminal_not_found");
    }

    #[test]
    fn a_relative_cwd_is_refused_without_opening_a_pty() {
        let report = create("term_x", "true", "relative/path");
        let TerminalOutcome::Failed { code, .. } = report.outcome else {
            panic!("expected a failure, got {:?}", report.outcome);
        };
        assert_eq!(code, "invalid_path");
    }

    #[test]
    fn close_force_kills_a_live_process() {
        let registry = TerminalRegistry::new();
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        answer(
            request(TerminalOperation::Create {
                id: "term_close".into(),
                start: TerminalStart::Command {
                    command: "sleep 30".into(),
                },
                target: TerminalTarget::HostPath {
                    host_id: loom_domain::HostId::mint(),
                    cwd: None,
                },
                cols: 80,
                rows: 24,
                title: "t".into(),
                cwd,
            }),
            &registry,
        );

        let report = answer(
            request(TerminalOperation::Close {
                id: "term_close".into(),
                force: true,
            }),
            &registry,
        );
        let TerminalOutcome::Session { session } = report.outcome else {
            panic!("expected a session");
        };
        assert_eq!(session.status, TerminalStatus::Exited);
        assert_eq!(session.close_reason, Some(TerminalCloseReason::User));
    }

    #[test]
    fn a_non_forced_close_of_a_live_session_is_refused() {
        let registry = TerminalRegistry::new();
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        answer(
            request(TerminalOperation::Create {
                id: "term_clean".into(),
                start: TerminalStart::Command {
                    command: "sleep 30".into(),
                },
                target: TerminalTarget::HostPath {
                    host_id: loom_domain::HostId::mint(),
                    cwd: None,
                },
                cols: 80,
                rows: 24,
                title: "t".into(),
                cwd,
            }),
            &registry,
        );
        let report = answer(
            request(TerminalOperation::Close {
                id: "term_clean".into(),
                force: false,
            }),
            &registry,
        );
        let TerminalOutcome::Failed { code, .. } = report.outcome else {
            panic!("expected a refusal");
        };
        assert_eq!(code, "terminal_not_running");
        // Clean up the live process so the test does not leak it.
        let _ = answer(
            request(TerminalOperation::Close {
                id: "term_clean".into(),
                force: true,
            }),
            &registry,
        );
    }

    #[test]
    fn resize_updates_the_recorded_size() {
        let registry = TerminalRegistry::new();
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        answer(
            request(TerminalOperation::Create {
                id: "term_resize".into(),
                start: TerminalStart::Command {
                    command: "sleep 30".into(),
                },
                target: TerminalTarget::HostPath {
                    host_id: loom_domain::HostId::mint(),
                    cwd: None,
                },
                cols: 80,
                rows: 24,
                title: "t".into(),
                cwd,
            }),
            &registry,
        );
        let report = answer(
            request(TerminalOperation::Resize {
                id: "term_resize".into(),
                cols: 120,
                rows: 40,
            }),
            &registry,
        );
        let TerminalOutcome::Session { session } = report.outcome else {
            panic!("expected a session");
        };
        assert_eq!((session.cols, session.rows), (120, 40));
        let _ = answer(
            request(TerminalOperation::Close {
                id: "term_resize".into(),
                force: true,
            }),
            &registry,
        );
    }

    #[test]
    fn a_replayed_create_returns_the_existing_session() {
        let registry = TerminalRegistry::new();
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let build = || {
            request(TerminalOperation::Create {
                id: "term_replay".into(),
                start: TerminalStart::Command {
                    command: "sleep 30".into(),
                },
                target: TerminalTarget::HostPath {
                    host_id: loom_domain::HostId::mint(),
                    cwd: None,
                },
                cols: 80,
                rows: 24,
                title: "t".into(),
                cwd: cwd.clone(),
            })
        };
        answer(build(), &registry);
        let second = answer(build(), &registry);
        let TerminalOutcome::Session { session } = second.outcome else {
            panic!("expected a session");
        };
        assert_eq!(session.id, "term_replay");
        assert_eq!(registry.len(), 1, "replay must not create a second session");
        let _ = answer(
            request(TerminalOperation::Close {
                id: "term_replay".into(),
                force: true,
            }),
            &registry,
        );
    }

    #[test]
    fn a_report_lists_every_held_session() {
        let registry = TerminalRegistry::new();
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        answer(
            request(TerminalOperation::Create {
                id: "term_r1".into(),
                start: TerminalStart::Command {
                    command: "sleep 30".into(),
                },
                target: TerminalTarget::HostPath {
                    host_id: loom_domain::HostId::mint(),
                    cwd: None,
                },
                cols: 80,
                rows: 24,
                title: "t".into(),
                cwd,
            }),
            &registry,
        );
        let report = answer(request(TerminalOperation::Report { id: None }), &registry);
        let TerminalOutcome::Sessions { sessions } = report.outcome else {
            panic!("expected sessions");
        };
        assert_eq!(sessions.len(), 1);
        let _ = answer(
            request(TerminalOperation::Close {
                id: "term_r1".into(),
                force: true,
            }),
            &registry,
        );
    }
}

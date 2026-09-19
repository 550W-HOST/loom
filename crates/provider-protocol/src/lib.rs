//! The provider execution contract: what the control plane dispatches to a
//! worker, and what a worker reports back.
//!
//! This is loom's answer to bb's `packages/host-daemon-contract`, scoped to the
//! one thing that matters here: **running a provider for a thread**. It is a
//! plain-data crate with no runtime, no IO and no relay dependency, so both the
//! Rust control plane and any future Node execution plane can depend on it
//! without dragging a runtime along.
//!
//! # The two directions
//!
//! ```text
//!   server ── RunDispatch ──▶ relay host:{id} ──▶ worker
//!   server ◀── ProviderReport ── worker socket
//! ```
//!
//! * [`RunDispatch`] travels **through the relay**, published to the target
//!   host's scope. That is what gives dispatch replay: a worker that was
//!   disconnected while a run was dispatched receives it on reconnect. The
//!   `run_id` is the idempotency key, so a redelivery of a run the worker
//!   already started is dropped rather than run twice.
//! * [`ProviderReport`] travels **up the worker's own socket**. A report is an
//!   observation, not a command: the server turns it into a
//!   [`loom_domain::DomainEvent::ThreadRunEvent`] and publishes it to the thread
//!   scope through the relay, so it is replayable like any other event.
//!
//! # Why the payload is opaque
//!
//! A dispatch carries only what the ACP agent needs. The worker translates ACP
//! notifications into [`RunEvent`] before it is reported; the control plane
//! never parses provider output. This keeps an agent's wire format out of the
//! server and the client contract.
//!
//! [`RunEvent`]: loom_domain::RunEvent

use loom_domain::{
    catalog::ProviderCatalog, AutomationId, AutomationRunId, EnvironmentId, HostId,
    HostPermissionMode, ProjectId, ReasoningLevel, RunEvent, RunId, ScriptInterpreter, ThreadId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How to reach an ACP agent.
///
/// The provider contract carries launch metadata, not a provider wire format.
/// The worker owns the ACP client and the agent owns its session storage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSpec {
    /// Stable provider name, for example `"pi"`. Echoed in run events.
    pub name: String,
    /// How the worker should reach this agent.
    ///
    /// Defaults to [`ProviderLaunch::AcpStdio`] for an explicitly named command.
    /// The built-in [`ProviderSpec::pi`] constructor selects the embedded
    /// `pi-acp` path explicitly. A missing launch value is therefore never a
    /// request to speak Pi's private JSON-RPC protocol.
    #[serde(default)]
    pub launch: ProviderLaunch,
    /// The executable to spawn.
    pub command: String,
    /// Arguments, in order.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory for the provider, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl ProviderSpec {
    /// Pi through the embedded `pi-acp` ACP agent.
    pub fn pi() -> Self {
        Self::acp_pi()
    }

    /// An agent reached over ACP, spawned as a child process.
    pub fn acp(command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            name: "acp".into(),
            launch: ProviderLaunch::AcpStdio,
            command: command.into(),
            args,
            cwd: None,
        }
    }

    /// Pi reached through `pi-acp` linked into the worker.
    ///
    /// Only `pi` itself is a child process; the adapter is in-process, which is
    /// why this names no command.
    pub fn acp_pi() -> Self {
        Self {
            name: "pi".into(),
            launch: ProviderLaunch::AcpEmbeddedPi,
            command: "pi".into(),
            args: Vec::new(),
            cwd: None,
        }
    }

    /// The program plus arguments, as argv.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = Vec::with_capacity(self.args.len() + 1);
        argv.push(self.command.clone());
        argv.extend(self.args.iter().cloned());
        argv
    }
}

impl Default for ProviderSpec {
    fn default() -> Self {
        Self::pi()
    }
}

/// How an ACP agent is reached.
///
/// ACP is the only provider protocol. The two variants differ only in where
/// the ACP agent lives: a native agent is a child process, while Pi's
/// `pi-acp::AcpAgent` is linked into the worker and connected in-process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderLaunch {
    /// An ACP agent spawned as a child process and spoken to over stdio.
    #[default]
    AcpStdio,
    /// Agent Client Protocol against `pi-acp` linked into the worker.
    ///
    /// Nothing is spawned for the adapter itself; only `pi` is a child.
    AcpEmbeddedPi,
}

/// A request to run one provider turn for one thread.
///
/// Published to `host:{host_id}` through the relay. Every field the worker
/// needs to run in isolation is present: the worker never has to call back for
/// context before it can start, which keeps the control plane out of the
/// dispatch path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDispatch {
    /// Idempotency key and the run's identity.
    pub run_id: RunId,
    /// The thread being advanced.
    pub thread_id: ThreadId,
    /// Its project, carried so events need no lookup.
    pub project_id: ProjectId,
    /// The host expected to execute it.
    pub host_id: HostId,
    /// The user turn that started the run.
    pub prompt: String,
    /// How to start the provider.
    pub provider: ProviderSpec,
    /// The agent's identifier for this thread's conversation, when one is
    /// already known.
    ///
    /// The control plane learned it from the thread's `thread/identity` event.
    /// Present means the worker should *continue* that conversation; absent
    /// means this is the thread's first run. Carried on the dispatch rather
    /// than looked up by the worker so a run still needs no callback before it
    /// can start — the property that keeps the control plane out of the
    /// execution path.
    ///
    /// Additive on the wire: an older dispatch deserializes with `None`, which
    /// means "start fresh", the behaviour it already had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
    /// The model the client chose for this thread, when it chose one.
    ///
    /// Opaque to the control plane: it is whatever the provider's advertised
    /// model catalogue offered, carried to the worker so the agent can be told
    /// which model to use. A value the agent does not recognise is the agent's
    /// to refuse, and a refusal is not a run failure.
    ///
    /// Additive on the wire: an older dispatch deserializes with `None`, which
    /// means "the agent's own default", the behaviour it already had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// How much reasoning the client asked for, when it asked for any.
    ///
    /// loom's own closed set rather than the agent's: the worker maps it onto
    /// the level the agent advertised for the session, and a level with no
    /// counterpart there is left alone rather than guessed at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_level: Option<ReasoningLevel>,
    /// The host policy ceiling applied to provider permission answers.
    #[serde(default)]
    pub permission_ceiling: HostPermissionMode,
    /// The wall-clock milliseconds by which the run must have a terminal event.
    pub deadline_ms: u64,
    /// When the control plane minted the dispatch.
    pub created_at_ms: u64,
}

/// A request to provision a managed environment's workspace on a host.
///
/// Like [`RunDispatch`] this travels **through the relay**, published to the
/// target host's scope, so a worker that was disconnected while it was sent
/// still receives it on reconnect. The worker owns the directory layout and
/// chooses the actual path under its configured workspace root; the control
/// plane only learns it from [`EnvironmentProvisionReport`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentProvision {
    /// The environment to provision.
    pub environment_id: EnvironmentId,
    /// Its project, carried so a report needs no lookup.
    pub project_id: ProjectId,
    /// The host expected to provision it.
    pub host_id: HostId,
    /// Wall-clock milliseconds when the control plane minted the request.
    pub created_at_ms: u64,
}

/// A provider's request for permission, on its way to the control plane.
///
/// This is the upward half of the interaction bridge, and it travels the same
/// way a [`ProviderReport`] does — up the worker's own socket, as an
/// observation the control plane turns into a durable entity. It is a request
/// rather than a report, so it gets its own frame: a `ProviderReport` wraps a
/// [`loom_domain::RunEvent`], and a question is not a run event.
///
/// `request_id` is the **worker's** identity for the request, not the
/// interaction id the control plane mints. The worker needs a stable key to
/// wait on before the control plane has answered anything, and the control
/// plane needs a value a redelivered frame deduplicates on; one string serves
/// both. The answer comes back naming this value, not loom's interaction id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InteractionRequest {
    /// The host asking. The server rejects a request for a run this host does
    /// not own, exactly as it does for a report.
    pub host_id: HostId,
    /// The run whose turn is blocked on the answer.
    pub run_id: RunId,
    /// The thread the run is advancing.
    pub thread_id: ThreadId,
    /// Its project, carried so the record needs no lookup.
    pub project_id: ProjectId,
    /// The worker's identity for this request, stable across redelivery.
    pub request_id: String,
    /// The agent's session id, when it has named one. This is what a client
    /// correlates the question with the `providerThreadId` on run events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_thread_id: Option<String>,
    /// What kind of request it is. ACP's permission request is an
    /// [`loom_domain::InteractionKind::Approval`]; the field is generic because
    /// the bridge is not ACP-specific.
    pub kind: loom_domain::InteractionKind,
    /// The request body, in the contract's payload shape.
    pub payload: loom_domain::InteractionPayload,
    /// When the request expires, if the agent set a limit. ACP's permission tool
    /// call can carry a `timeout_ms`, and the control plane records it so a
    /// client can render a countdown and a reaper can settle it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
}

/// A permission decision the control plane maps onto the agent's own options.
///
/// The three words are bb's contract vocabulary, deliberately narrower than
/// ACP's `PermissionOptionKind` (which adds a reject-always). The worker maps a
/// word onto whichever option of that kind the agent offered — see
/// `crates/worker/src/acp/permission.rs` and its note on what a polarity cannot
/// express.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Grant this one operation.
    AllowOnce,
    /// Grant it for the rest of this agent session.
    AllowForSession,
    /// Refuse it.
    Deny,
}

/// The answer to an [`InteractionRequest`], on its way back to the worker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InteractionAnswer {
    /// A permission decision, mapped onto the agent's own options by the worker
    /// (an `allow` picks an allowing option, a `deny` a rejecting one).
    Decision {
        /// The decision.
        decision: PermissionDecision,
    },
    /// Settled without an answer: the run stopped, or no client answered and
    /// the request was withdrawn. The worker must **not** read this as
    /// acceptance.
    Cancelled {
        /// Why, for the worker's log.
        reason: String,
    },
}

/// A resolved interaction, published to `host:{id}` through the relay.
///
/// Downward, this travels exactly like a [`RunDispatch`] — through the relay to
/// the target host's scope — so a worker that was reconnecting when the user
/// answered still receives it on replay. The worker matches `request_id`
/// against the permission request it is holding open; a resolution for a
/// request it is not waiting on is dropped, which makes redelivery idempotent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InteractionResolutionFrame {
    /// The host that owns the run.
    pub host_id: HostId,
    /// The run whose turn asked.
    pub run_id: RunId,
    /// The thread it is advancing.
    pub thread_id: ThreadId,
    /// The worker's own identity for the request, echoed back so the worker can
    /// match it without sharing loom's id derivation.
    pub request_id: String,
    /// The control plane's interaction id, for logging and for a client that
    /// correlates the two.
    pub interaction_id: String,
    /// The answer.
    pub answer: InteractionAnswer,
    /// Wall-clock milliseconds when the control plane published it.
    pub created_at_ms: u64,
}

/// What a worker did with an [`EnvironmentProvision`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum EnvironmentProvisionOutcome {
    /// The workspace exists and is usable at `path`.
    Provisioned {
        /// Absolute path the worker created.
        path: String,
    },
    /// Provisioning failed; the environment moves to `error`.
    Failed {
        /// Why, verbatim, so it can be shown to a user.
        error: String,
    },
}

/// A worker's report about one provisioning attempt.
///
/// Sent up the worker's own socket, exactly like [`ProviderReport`]: the
/// server turns it into environment status events and publishes them to the
/// project scope through the relay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentProvisionReport {
    /// The host making the report.
    pub host_id: HostId,
    /// The environment being provisioned.
    pub environment_id: EnvironmentId,
    /// What happened.
    pub outcome: EnvironmentProvisionOutcome,
}

/// A worker's observation about an in-flight run.
///
/// `host_id` is what lets the server reject a report for a run this connection
/// is not allowed to speak for; it must match the host the socket enrolled as.
/// The [`RunEvent`] already carries the run and thread identity (and the
/// bb-contract event), so those are not repeated here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderReport {
    /// The host making the report.
    pub host_id: HostId,
    /// What happened.
    pub event: RunEvent,
}

/// A worker's report of what the agent on its machine can run.
///
/// This is a fact about the **host**, not about a run: which models the agent
/// advertises and which thinking-level ladder belongs to each of them. It is
/// therefore its own frame rather than a field on a run event, and the server
/// keeps it per host. The worker reads it once at enrollment and again from
/// every session it opens, so a model list that changes under a stored choice
/// is corrected on the next turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCatalogReport {
    /// The host whose agent this catalogue describes.
    pub host_id: HostId,
    /// The models the agent advertised, and the one its session is on.
    pub catalog: ProviderCatalog,
}

// ---------------------------------------------------------------------------
// Host file access
// ---------------------------------------------------------------------------
//
// A thread's files live on the machine that owns its environment, and the
// control plane must not read its *own* disk and call it a host file. So a file
// read or a directory listing is a request to the host, and it travels the two
// paths the worker already has:
//
// ```text
//   server ── HostFileRequest ──▶ relay host:{id} ──▶ worker
//   server ◀── HostFileReport ── worker socket
// ```
//
// The request goes through the relay for the same reason a dispatch does: a
// worker that was reconnecting still receives it on replay, and a replayed
// read is harmless because reading is idempotent. The report comes back up the
// worker's own socket, because it answers exactly one request and must not be
// fanned out to every client watching the room.
//
// `request_id` is a correlation token, not a domain id: the server mints it,
// keeps the waiting HTTP request keyed by it, and drops a report whose token it
// no longer knows (a redelivered or abandoned request).

/// What a host was asked to do with its filesystem.
///
/// Deliberately a small set of operations over one code path, so the
/// containment, hidden-file and limit policies cannot drift between them.
/// Write and copy live here rather than in their own request/report pair
/// because they answer the same question — "operate on a file on the machine
/// that owns it" — and share the same correlation, host-ownership and size
/// bounds. See `docs/contract.md` ("B7").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum HostFileOperation {
    /// Read one file, at most `max_bytes` of it.
    Read {
        /// Absolute path on the host.
        path: String,
        /// When set, the *real* resolved path must stay inside this root.
        ///
        /// The control plane joins a root and a client-supplied relative path
        /// before sending it, so this is the second half of the traversal
        /// defence: the worker re-checks containment against symlinks that
        /// neither side could see when the path was assembled.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root_path: Option<String>,
        /// Upper bound on the file size. A larger file is refused, not
        /// truncated: half a file is not the file the client asked for.
        max_bytes: u64,
    },
    /// List only the direct children of one directory.
    ListDirectory {
        /// Absolute path of the directory on the host.
        path: String,
        /// Whether files are candidates.
        include_files: bool,
        /// Whether directories are candidates.
        include_directories: bool,
        /// Whether dotfiles are candidates.
        include_hidden: bool,
        /// Maximum entries returned.
        limit: usize,
    },
    /// List the entries under one directory, recursively.
    List {
        /// Absolute path of the directory on the host.
        path: String,
        /// Fuzzy filter; `None` lists everything up to `limit`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        /// Maximum entries returned. Truncation is reported, never silent.
        limit: usize,
        /// Whether files are candidates.
        include_files: bool,
        /// Whether directories are candidates.
        include_directories: bool,
        /// Whether dotfiles are candidates.
        include_hidden: bool,
    },
    /// Write one file inside `root_path`, creating parent directories.
    ///
    /// The root is **required and absolute**. An upload lands inside a
    /// directory the control plane derived from the project's own workspace, so
    /// a caller cannot name an arbitrary path on the host: the worker resolves
    /// the real path and refuses anything outside the root, exactly as a read
    /// does. `content` is base64, so binary uploads survive the JSON hop.
    ///
    /// When `overwrite` is false and the target exists, the write succeeds at a
    /// suffixed sibling (`name-2.ext`) instead of clobbering it; the answer
    /// names the path actually written. That is what the read side already does
    /// for a colliding copy, and an upload that silently replaced a file the
    /// user still had would be the same data loss.
    Write {
        /// Absolute path to write.
        path: String,
        /// Absolute directory the resolved path must stay inside.
        root_path: String,
        /// The bytes, base64-encoded.
        content: String,
        /// Upper bound on the decoded size, refused rather than truncated.
        max_bytes: u64,
        /// Whether an existing file is replaced. `false` writes a suffixed
        /// sibling instead.
        overwrite: bool,
    },
    /// Check whether a bounded set of absolute paths exists on the host.
    Exists {
        /// Absolute paths to inspect.
        paths: Vec<String>,
    },
    /// Create one directory, optionally with its parents.
    ///
    /// `root_path` is optional and, when given, is the containment boundary
    /// the worker re-checks on the *resolved* path. A create that names no
    /// root is confined only by being absolute, which is what the reference
    /// client does when it creates a directory it just picked in a host dialog.
    CreateDirectory {
        /// Absolute directory to create.
        path: String,
        /// When set, the resolved path must stay inside this absolute root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root_path: Option<String>,
        /// Whether missing parent directories are created too.
        recursive: bool,
    },
    /// Move (rename) one path to another on the same host.
    Move {
        /// Absolute source path.
        source_path: String,
        /// Absolute destination path.
        destination_path: String,
        /// When set, both resolved paths must stay inside this absolute root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root_path: Option<String>,
        /// When false, an existing destination is refused rather than
        /// replaced. The move API's callers expect a rename, not a merge, and
        /// a silent overwrite would lose a file the user still had.
        overwrite: bool,
    },
    /// Remove one file or directory, optionally recursively.
    Remove {
        /// Absolute path to remove.
        path: String,
        /// When set, the resolved path must stay inside this absolute root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root_path: Option<String>,
        /// Whether a directory is removed with its contents. A non-empty
        /// directory without this flag is refused, never partially removed.
        recursive: bool,
    },
    /// Read one file with optimistic-concurrency metadata.
    ///
    /// This is the richer sibling of [`HostFileOperation::Read`]: it answers a
    /// content hash so a client can write it back with an `expected_sha256`,
    /// and it refuses a directory instead of failing obscurely. `Read` stays
    /// for the older observers whose contract carries no hash.
    ReadWithMetadata {
        /// Absolute path on the host.
        path: String,
        /// When set, the *real* resolved path must stay inside this root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root_path: Option<String>,
        /// Upper bound on the file size; larger files are refused, not
        /// truncated.
        max_bytes: u64,
    },
    /// Write one file with optimistic concurrency and POSIX mode control.
    ///
    /// Distinct from [`HostFileOperation::Write`], which exists for uploads
    /// that must never clobber a colliding name. This one is the editor's
    /// write: it replaces the target, honours `create_parents`, honouring an
    /// optional file mode, and refuses the write when `expected_sha256` does
    /// not match what is on disk right now — which is how an editor's save
    /// detects a concurrent change instead of silently overwriting it.
    WriteFile {
        /// Absolute path to write.
        path: String,
        /// Absolute directory the resolved path must stay inside.
        root_path: String,
        /// The bytes, encoded per `content_encoding`.
        content: String,
        /// How `content` is encoded.
        content_encoding: HostFileEncoding,
        /// Upper bound on the decoded size, refused rather than truncated.
        max_bytes: u64,
        /// Whether missing parent directories are created.
        create_parents: bool,
        /// When set, the file is refused unless its current SHA-256 equals
        /// this value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_sha256: Option<String>,
        /// When true, the write is refused unless the target does not exist.
        ///
        /// This is the `expectedSha256: null` case of the HTTP contract: a
        /// client creating a new file wants create-only semantics, which is a
        /// different assertion from "the bytes hash to X".
        #[serde(default)]
        create_only: bool,
        /// POSIX mode bits to apply after writing, when the platform has them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
    },
    /// Modify a file's mode bits and/or its modification time.
    ///
    /// The touch half of an editor's save cycle. Both fields are optional and
    /// at least one must be present; a request that names neither is refused
    /// rather than reported as a silent success.
    SetMetadata {
        /// Absolute path on the host.
        path: String,
        /// When set, the resolved path must stay inside this absolute root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root_path: Option<String>,
        /// POSIX mode bits to apply, when the platform has them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
        /// Whether to update the modification time to now.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        touch: Option<bool>,
    },
    /// Copy a single path to a destination path, confined to a shared root.
    CopyPath {
        /// Absolute source path.
        source_path: String,
        /// Absolute destination path.
        destination_path: String,
        /// Absolute directory both resolved paths must stay inside.
        root_path: String,
        /// Whether an existing destination is replaced.
        overwrite: bool,
    },
    /// Copy existing files into a destination directory.
    ///
    /// Two roots, deliberately: a project-to-project attachment copy reads
    /// from one project's attachment directory and writes into another's, and
    /// a single root cannot describe both. Every source must resolve inside
    /// `source_root` and the destination inside `destination_root`, so a copy
    /// can never read or write outside the two attachment directories it was
    /// told about. Sources that are missing or oversized are reported per path
    /// rather than failing the whole request, because a client copying several
    /// attachments should be told which ones did not make it.
    Copy {
        /// Absolute paths to copy, in order.
        paths: Vec<String>,
        /// Absolute directory the sources must stay inside.
        source_root: String,
        /// Absolute destination directory; created if absent.
        destination: String,
        /// Absolute directory the destination must stay inside.
        destination_root: String,
        /// Per-file upper bound, refused rather than truncated.
        max_bytes: u64,
    },
}

/// A request for a host to read, list or write its own filesystem.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostFileRequest {
    /// Correlation token; the server is waiting on exactly this value.
    pub request_id: String,
    /// The host expected to answer.
    pub host_id: HostId,
    /// What to do.
    pub operation: HostFileOperation,
    /// Wall-clock milliseconds when the control plane minted the request.
    pub created_at_ms: u64,
}

/// How a file's bytes were encoded for transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostFileEncoding {
    /// The bytes are valid UTF-8, so they travel as the string itself.
    Utf8,
    /// The bytes are not UTF-8; they travel base64-encoded.
    Base64,
}

/// One file's contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostFileContent {
    /// Absolute path that was read, echoed so the caller cannot confuse it with
    /// the root-relative path it asked for.
    pub path: String,
    /// The bytes, in [`HostFileContent::content_encoding`].
    pub content: String,
    /// How to decode [`HostFileContent::content`].
    pub content_encoding: HostFileEncoding,
    /// Size of the file in bytes, which is *not* the length of `content` when
    /// it is base64.
    pub size_bytes: u64,
    /// Best-effort media type from the path's extension, so the caller can set
    /// a `content-type` without a second guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Wall-clock milliseconds of the file's last modification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at_ms: Option<u64>,
    /// SHA-256 of the bytes on disk, lowercase hex.
    ///
    /// Additive on the wire: an older worker omits it and a reader that needs a
    /// hash asks for [`HostFileOperation::ReadWithMetadata`] instead. It is
    /// present on a write's answer so the `files.write` route can report the
    /// hash the contract requires without a second round trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// Whether a listed entry is a file or a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostPathKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
}
/// One listed entry, relative to the listed root.
///
/// `score` and `positions` exist for the contract's fuzzy path list; with no
/// query they are `0` and empty, exactly as bb reports an unfiltered listing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostFileEntry {
    /// Path relative to the listed root, with `/` separators on every platform.
    pub path: String,
    /// The final path segment.
    pub name: String,
    /// File or directory.
    pub kind: HostPathKind,
    /// Relevance for the query, higher first. `0` when there was no query.
    pub score: f64,
    /// Character offsets in `path` that matched, for highlighting.
    pub positions: Vec<usize>,
}

/// What a host answered.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum HostFileOutcome {
    /// One file's contents.
    Content(HostFileContent),
    /// A directory listing.
    Listing {
        /// Matching entries, best-first when a query was given.
        entries: Vec<HostFileEntry>,
        /// Whether entries were dropped to honour the limit.
        truncated: bool,
    },
    /// A file was written; the entry describes what now exists on disk.
    Written(HostFileContent),
    /// One file's contents together with the metadata a writer needs to
    /// detect a concurrent change: a SHA-256 of the bytes and its mode.
    FileMetadata {
        /// The bytes, encoded per `content_encoding`.
        content: String,
        /// How `content` is encoded.
        content_encoding: HostFileEncoding,
        /// Number of bytes on disk, independent of the encoding.
        size_bytes: u64,
        /// SHA-256 of the bytes, lowercase hex.
        sha256: String,
        /// POSIX mode bits, when the platform reports them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
        /// Wall-clock milliseconds of the last modification.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        modified_at_ms: Option<u64>,
    },
    /// A write was refused because the file changed under the writer.
    Conflict {
        /// The hash that was found, or `None` when the file does not exist.
        current_sha256: Option<String>,
    },
    /// A path-affecting operation (mkdir, move, remove, metadata, copy) that
    /// has no natural content to return.
    Done,
    /// Files were copied, and the entries describe the copies.
    Copied {
        /// The copies that were made, in the order they were asked for.
        files: Vec<HostFileContent>,
        /// Sources that could not be copied, with the reason.
        failures: Vec<HostFileFailure>,
    },
    /// The request could not be carried out.
    Failed {
        /// A stable machine-readable code, in the vocabulary the HTTP layer
        /// already uses (`not_found`, `invalid_path`, `file_too_large`, …).
        code: String,
        /// A human-readable explanation.
        message: String,
    },
}

/// One source that a bounded copy could not take.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostFileFailure {
    /// The source path that failed, echoed verbatim.
    pub path: String,
    /// A stable machine-readable code.
    pub code: String,
    /// A human-readable explanation.
    pub message: String,
}

/// A host's answer to one [`HostFileRequest`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostFileReport {
    /// The host answering.
    pub host_id: HostId,
    /// The request being answered, echoed verbatim.
    pub request_id: String,
    /// What happened.
    pub outcome: HostFileOutcome,
}

// ---------------------------------------------------------------------------
// Host workspace RPCs
// ---------------------------------------------------------------------------
//
// Workspace state belongs to the machine that owns the environment. These
// requests use the same relay-plus-report shape as host file access, but keep
// git execution and provider credentials entirely on that machine.

/// The workspace path a host RPC is allowed to inspect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceContext {
    /// Absolute path on the enrolled host.
    #[serde(rename = "workspacePath")]
    pub workspace_path: String,
}

/// A git comparison target shared by workspace diff operations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspaceDiffTarget {
    /// Changes in the working tree relative to `HEAD`.
    Uncommitted,
    /// Commits on the current branch relative to a merge-base branch.
    BranchCommitted {
        #[serde(rename = "mergeBaseBranch")]
        merge_base_branch: String,
    },
    /// Both committed and uncommitted changes relative to a merge-base branch.
    All {
        #[serde(rename = "mergeBaseBranch")]
        merge_base_branch: String,
    },
    /// One commit, compared with its first parent when one exists.
    Commit { sha: String },
}

/// Which side of a diff file is requested.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceDiffFileSide {
    Old,
    New,
}

/// A request sent to the host that owns a workspace.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HostRpcOperation {
    /// Inspect a repository source used by a project.
    #[serde(rename = "host.inspect_git_source")]
    InspectGitSource {
        path: String,
        #[serde(rename = "remoteRefresh")]
        remote_refresh: String,
    },
    /// List local and remote branches for a repository source.
    #[serde(rename = "host.list_branch_options")]
    ListBranchOptions {
        path: String,
        limit: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        #[serde(
            rename = "selectedBranch",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        selected_branch: Option<String>,
        #[serde(rename = "remoteRefresh")]
        remote_refresh: String,
    },
    /// Read the complete workspace status.
    #[serde(rename = "workspace.status")]
    WorkspaceStatus {
        #[serde(rename = "environmentId")]
        environment_id: EnvironmentId,
        #[serde(rename = "workspaceContext")]
        workspace_context: WorkspaceContext,
        #[serde(
            rename = "mergeBaseBranch",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        merge_base_branch: Option<String>,
        #[serde(rename = "maxUntrackedLineStatFiles")]
        max_untracked_line_stat_files: u64,
        #[serde(rename = "maxUntrackedLineStatBytes")]
        max_untracked_line_stat_bytes: u64,
    },
    /// Read an aggregate diff.
    #[serde(rename = "workspace.diff")]
    WorkspaceDiff {
        #[serde(rename = "environmentId")]
        environment_id: EnvironmentId,
        #[serde(rename = "workspaceContext")]
        workspace_context: WorkspaceContext,
        target: WorkspaceDiffTarget,
        #[serde(rename = "maxDiffBytes")]
        max_diff_bytes: u64,
        #[serde(rename = "maxFileListBytes")]
        max_file_list_bytes: u64,
        #[serde(rename = "maxUntrackedFiles")]
        max_untracked_files: u64,
    },
    /// Read changed-file metadata.
    #[serde(rename = "workspace.diffFiles")]
    WorkspaceDiffFiles {
        #[serde(rename = "environmentId")]
        environment_id: EnvironmentId,
        #[serde(rename = "workspaceContext")]
        workspace_context: WorkspaceContext,
        target: WorkspaceDiffTarget,
        #[serde(rename = "maxFiles")]
        max_files: u64,
    },
    /// Read patches for a bounded set of paths.
    #[serde(rename = "workspace.diffPatch")]
    WorkspaceDiffPatch {
        #[serde(rename = "environmentId")]
        environment_id: EnvironmentId,
        #[serde(rename = "workspaceContext")]
        workspace_context: WorkspaceContext,
        target: WorkspaceDiffTarget,
        paths: Vec<String>,
        #[serde(rename = "maxBytesPerFile")]
        max_bytes_per_file: u64,
    },
    /// Read one side of one changed file.
    #[serde(rename = "workspace.diffFile")]
    WorkspaceDiffFile {
        #[serde(rename = "environmentId")]
        environment_id: EnvironmentId,
        #[serde(rename = "workspaceContext")]
        workspace_context: WorkspaceContext,
        target: WorkspaceDiffTarget,
        path: String,
        side: WorkspaceDiffFileSide,
        #[serde(rename = "maxBytes")]
        max_bytes: u64,
    },
    /// Inspect the pull request associated with the checked-out branch.
    #[serde(rename = "workspace.pull_request")]
    WorkspacePullRequest {
        #[serde(rename = "environmentId")]
        environment_id: EnvironmentId,
        #[serde(rename = "workspaceContext")]
        workspace_context: WorkspaceContext,
    },
    /// Commit all current workspace changes.
    #[serde(rename = "workspace.commit")]
    WorkspaceCommit {
        #[serde(rename = "environmentId")]
        environment_id: EnvironmentId,
        #[serde(rename = "workspaceContext")]
        workspace_context: WorkspaceContext,
        message: String,
    },
    /// Change the associated pull request's draft/ready/merge state.
    #[serde(rename = "workspace.pull_request_action")]
    WorkspacePullRequestAction {
        #[serde(rename = "environmentId")]
        environment_id: EnvironmentId,
        #[serde(rename = "workspaceContext")]
        workspace_context: WorkspaceContext,
        operation: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<String>,
    },
    /// Ask the host-side UI capability to select a folder. Headless workers
    /// answer with a null path rather than pretending the server has a dialog.
    #[serde(rename = "host.pick_folder")]
    PickFolder {
        #[serde(rename = "clientHostId")]
        client_host_id: String,
    },
    /// Compute the worker's configured default clone directory.
    #[serde(rename = "host.clone_default_path")]
    CloneDefaultPath {
        #[serde(rename = "projectId")]
        project_id: ProjectId,
    },
    /// List the prompt commands available in a project workspace.
    ///
    /// A command list is a property of the **workspace on disk** — project
    /// prompts live under `<cwd>/.pi/prompts`, user prompts under the agent
    /// data directory — so the machine that owns the workspace is the only one
    /// that can answer it. The result is projected into bb's
    /// `projectCommandSchema` shape by the control plane; this operation
    /// carries only the working directory and answers raw command rows.
    #[serde(rename = "host.list_commands")]
    ListCommands {
        #[serde(rename = "cwd")]
        cwd: String,
    },
}

/// A host-scoped workspace request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostRpcRequest {
    /// Correlation token minted by the server.
    pub request_id: String,
    /// The host expected to execute the operation.
    pub host_id: HostId,
    /// The operation and its bounded arguments.
    pub operation: HostRpcOperation,
    /// Wall-clock milliseconds when the request was created.
    pub created_at_ms: u64,
}

/// What a host returned for a workspace request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum HostRpcOutcome {
    /// JSON result in the operation's declared result shape.
    Result { result: Value },
    /// A bounded, machine-readable failure.
    Failed { code: String, message: String },
}

/// A host's answer to one [`HostRpcRequest`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostRpcReport {
    /// The host answering.
    pub host_id: HostId,
    /// The request being answered.
    pub request_id: String,
    /// The result or failure.
    pub outcome: HostRpcOutcome,
}

// ---------------------------------------------------------------------------
// Terminal sessions
// ---------------------------------------------------------------------------
//
// A terminal is a **process on the machine that owns it**, not a server-side
// abstraction. The control plane therefore keeps only what it needs to route a
// request — identity, ownership, size and status — and every byte of input,
// output and resize is a request to that host. The server never holds a PTY
// handle and never buffers an unbounded stdout.
//
// ```text
//   HTTP ── TerminalRequest ──▶ relay host:{id} ──▶ worker
//   HTTP ◀── TerminalReport ── worker socket
// ```
//
// This mirrors the file protocol deliberately: the same correlation token, the
// same host-ownership check, the same bounded answer. The difference is that a
// terminal is **stateful on the host**, so the request kind names the session
// and its answers are ordered by a per-session output sequence number.

/// What kind of thing a terminal was started as.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum TerminalStart {
    /// The host's default shell.
    Shell,
    /// One non-interactive command, run under the shell.
    Command {
        /// The command line, exactly as the user typed it.
        command: String,
    },
}

/// What a terminal was attached to, which also settles who owns it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TerminalTarget {
    /// A thread's workspace on `host_id`.
    Thread {
        /// The owning thread.
        #[serde(rename = "threadId")]
        thread_id: ThreadId,
    },
    /// A managed environment's workspace.
    Environment {
        /// The owning environment.
        #[serde(rename = "environmentId")]
        environment_id: EnvironmentId,
    },
    /// An absolute host path, optionally with its own host.
    HostPath {
        /// The host that owns the path. Required, because a bare path names no
        /// machine and this protocol never guesses one.
        #[serde(rename = "hostId")]
        host_id: HostId,
        /// The absolute working directory, or `None` for the host's default.
        #[serde(rename = "cwd", default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
}

/// The lifecycle state of a terminal, as the host reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalStatus {
    /// The process is spawning.
    Starting,
    /// The process is running and accepts input.
    Running,
    /// The host that owns it dropped while it was alive; the session is still
    /// recorded but cannot be driven until the host reconnects and reports it.
    Disconnected,
    /// The process ended. `exit_code` is meaningful only here.
    Exited,
}

/// Why a terminal ended, in the contract's vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalCloseReason {
    /// The client asked for it.
    User,
    /// The owning thread was deleted.
    ThreadDeleted,
    /// The process ended on its own.
    ProcessExit,
    /// The owning worker disconnected.
    WorkerDisconnect,
    /// The owning environment was destroyed.
    EnvironmentDestroyed,
    /// The owning thread was archived.
    ThreadArchived,
    /// A create or attach waited past its deadline.
    OpenTimeout,
}

/// The control plane's record of one terminal session.
///
/// This is the entity a client lists and reads; the worker is the authority on
/// whether the process it names is actually alive, and reconciles this record
/// through [`TerminalOperation::Report`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSession {
    /// The stable session id.
    pub id: String,
    /// The owning thread, when the target was a thread.
    #[serde(rename = "threadId", default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    /// The owning environment, when the target was an environment.
    #[serde(
        rename = "environmentId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub environment_id: Option<EnvironmentId>,
    /// The host that owns the process.
    #[serde(rename = "hostId")]
    pub host_id: HostId,
    /// Display title.
    pub title: String,
    /// Absolute working directory the process was started in.
    #[serde(rename = "initialCwd")]
    pub initial_cwd: String,
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
    /// Lifecycle state.
    pub status: TerminalStatus,
    /// Exit code, only meaningful when `status` is `exited`.
    #[serde(rename = "exitCode", default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Why it ended, when it has.
    #[serde(
        rename = "closeReason",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub close_reason: Option<TerminalCloseReason>,
    /// When it was created, in wall-clock milliseconds.
    #[serde(rename = "createdAt")]
    pub created_at_ms: u64,
    /// When it was last changed, in wall-clock milliseconds.
    #[serde(rename = "updatedAt")]
    pub updated_at_ms: u64,
    /// The last time a user sent it input, when they have.
    #[serde(
        rename = "lastUserInputAt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub last_user_input_at_ms: Option<u64>,
    /// The next output chunk the host will mint.
    ///
    /// The cursor a client resumes from. It is authoritative on the host (which
    /// owns the output ring) and mirrored here so `terminals.list` can answer
    /// without a round trip.
    #[serde(rename = "nextSeq")]
    pub next_seq: u64,
}

/// One chunk of terminal output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalOutputChunk {
    /// Monotonic per-session sequence number, starting at 0.
    pub seq: u64,
    /// The bytes, base64-encoded so binary terminal output survives JSON.
    #[serde(rename = "dataBase64")]
    pub data_base64: String,
}

/// A request sent to the host that owns a terminal session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum TerminalOperation {
    /// Start a new session. The worker mints nothing; the server already
    /// minted `id`, so a redelivered create is idempotent on that id.
    Create {
        /// The id the control plane assigned.
        id: String,
        /// Shell or one command.
        start: TerminalStart,
        /// What it is attached to, and therefore who owns it.
        target: TerminalTarget,
        /// Initial columns.
        cols: u16,
        /// Initial rows.
        rows: u16,
        /// Display title the client chose.
        title: String,
        /// Absolute working directory the worker resolves and reports back.
        cwd: String,
    },
    /// Send bytes to the process's stdin.
    Input {
        /// The session to write to.
        id: String,
        /// The bytes, base64-encoded.
        data_base64: String,
    },
    /// Change the window size and signal the child.
    Resize {
        /// The session to resize.
        id: String,
        /// New columns.
        cols: u16,
        /// New rows.
        rows: u16,
    },
    /// Read output from a cursor.
    Output {
        /// The session to read.
        id: String,
        /// Read chunks strictly after this sequence number.
        since_seq: u64,
        /// Maximum chunks to return.
        limit: usize,
        /// Upper bound on the total decoded bytes to return.
        tail_bytes: u64,
    },
    /// Close the session with a reason.
    Close {
        /// The session to close.
        id: String,
        /// Whether a live process is killed (`force`) or an already-exited
        /// session is only recorded (`if_clean`).
        force: bool,
    },
    /// Restart a session's process under the same id, resetting its output.
    Restart {
        /// The session to restart.
        id: String,
    },
    /// Ask the host to report the current state of every session it holds.
    ///
    /// Sent after a reconnect: the worker is the authority on liveness, so the
    /// control plane reconciles its records from this rather than guessing
    /// which processes survived.
    Report {
        /// Restrict the report to one session when set.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
}

/// A request to the host that owns a terminal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TerminalRequest {
    /// Correlation token; the server is waiting on exactly this value.
    pub request_id: String,
    /// The host expected to answer.
    pub host_id: HostId,
    /// What to do.
    pub operation: TerminalOperation,
    /// Wall-clock milliseconds when the control plane minted the request.
    pub created_at_ms: u64,
}

/// What a host answered for one terminal request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum TerminalOutcome {
    /// A session was created, restarted, closed or otherwise described.
    Session {
        /// The session's current state.
        session: TerminalSession,
    },
    /// Output was read.
    Output {
        /// The chunks, in sequence order.
        chunks: Vec<TerminalOutputChunk>,
        /// The cursor to pass as `since_seq` on the next read.
        next_seq: u64,
        /// Whether older chunks were dropped before this window.
        truncated: bool,
    },
    /// One or more sessions, for a reconciliation report.
    Sessions {
        /// Every session the host holds, or the one that was asked about.
        sessions: Vec<TerminalSession>,
    },
    /// The request could not be carried out.
    Failed {
        /// A stable machine-readable code, in the vocabulary the HTTP layer
        /// already uses (`terminal_not_found`, `terminal_not_running`, …).
        code: String,
        /// A human-readable explanation.
        message: String,
    },
}

/// A host's answer to one [`TerminalRequest`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TerminalReport {
    /// The host answering.
    pub host_id: HostId,
    /// The request being answered, echoed verbatim.
    pub request_id: String,
    /// What happened.
    pub outcome: TerminalOutcome,
}

// ---------------------------------------------------------------------------
// Automation scripts
// ---------------------------------------------------------------------------
//
// A script automation's script is a **process on the machine that owns the
// workspace**, so the control plane never runs one: it publishes a request to
// the host and turns the host's report into the run's result. The shape is the
// terminal channel's — a request through the relay, a report up the worker's
// own socket, and a correlation id — with one difference: this is a
// fire-and-forget job rather than a conversation, so the correlation id is the
// automation run itself and there is a cancel frame for it.
//
// ```text
//   server ── ScriptRunDispatch ──▶ relay host:{id} ──▶ worker
//   server ◀── ScriptRunReport ──── worker socket
//   server ── ScriptRunCancel ────▶ relay host:{id} ──▶ worker
// ```
//
// `run_id` is both the correlation token and the idempotency key: a redelivered
// dispatch of a run the worker already started is dropped, and a report for a
// run this connection does not own is refused — the same guarantees the
// provider path has.

/// A request to run one automation script on the host that owns its workspace.
///
/// Everything the host needs to run in isolation is here: the workspace to run
/// in, exactly one source for the script, the interpreter, the environment the
/// automation declared, and the timeout. The host never calls back for context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptRunDispatch {
    /// The automation run this executes. The idempotency key and the
    /// correlation id for the report.
    pub run_id: AutomationRunId,
    /// The automation it belongs to.
    pub automation_id: AutomationId,
    /// Its project, carried so a report needs no lookup.
    pub project_id: ProjectId,
    /// The host expected to run it.
    pub host_id: HostId,
    /// The workspace the script runs in: the environment the automation
    /// resolved to.
    pub cwd: String,
    /// The script body, inline. Exactly one of `script` and `script_file` is
    /// set; the host writes an inline body to its own data directory, which is
    /// why the report carries the path it used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// A path to the script, resolved *inside* `cwd`. The host re-checks
    /// containment against the real filesystem, so a symlink that leaves the
    /// workspace is refused there even if the control plane could not see it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_file: Option<String>,
    /// The interpreter to run it with. Absent means "the one the file's
    /// extension names, or `bash`".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interpreter: Option<ScriptInterpreter>,
    /// Environment variables the automation declared.
    ///
    /// This is the whole of what the script inherits from its owner: the host
    /// runs it with a cleared environment plus `PATH`, these, and the
    /// `LOOM_*` identity variables it adds itself. A script therefore cannot
    /// read the worker's own environment by accident.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    /// How long the process may run.
    pub timeout_ms: u64,
    /// Wall-clock milliseconds by which the control plane expects a report.
    pub deadline_ms: u64,
    /// When the control plane minted the dispatch.
    pub created_at_ms: u64,
}

/// What a host did with a [`ScriptRunDispatch`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ScriptRunOutcome {
    /// The process ran, or was killed by its timeout.
    Exited {
        /// What the process exited with. `None` means it was killed by a
        /// signal rather than exiting.
        exit_code: Option<i32>,
        /// The captured output, `stdout` then `stderr`, already truncated to
        /// the host's cap.
        output: String,
        /// Whether the cap cut the output short. Truncation is reported, never
        /// silent, and never a failure.
        output_truncated: bool,
        /// Whether the timeout killed it.
        timed_out: bool,
        /// The file the host ran, for an inline script it wrote itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        script_path: Option<String>,
    },
    /// The host refused to run it: no such file, a path outside the workspace,
    /// or no such interpreter. The run fails with this text.
    Refused {
        /// Why, verbatim, so it can be shown to a user.
        error: String,
    },
    /// The host killed it because the control plane asked.
    Cancelled {
        /// The output captured before the kill.
        output: String,
        /// Whether the cap cut that output short.
        output_truncated: bool,
    },
}

/// A host's report about one script run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptRunReport {
    /// The host making the report.
    pub host_id: HostId,
    /// The automation run being reported on, echoed verbatim.
    pub run_id: AutomationRunId,
    /// What happened.
    pub outcome: ScriptRunOutcome,
}

/// A request to stop a running script on its host.
///
/// Published to the host's scope exactly like the dispatch, so a cancel for a
/// worker that is reconnecting is delivered on replay. The worker kills the
/// process and reports [`ScriptRunOutcome::Cancelled`]; the control plane has
/// already settled the run, so a report that arrives afterwards is an
/// idempotent no-op.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptRunCancel {
    /// The run to stop.
    pub run_id: AutomationRunId,
    /// The host expected to be running it.
    pub host_id: HostId,
    /// Why, for the worker's log.
    pub reason: String,
    /// When the control plane published it.
    pub created_at_ms: u64,
}

/// Where a host writes the script of an automation run it was dispatched.
///
/// The sibling of [`thread_storage_root`], for the same reason: the layout
/// belongs to the worker and the control plane can only name it from the data
/// directory the host reported. It lives here so the path a report carries can
/// be understood by the side that stored it.
pub fn automation_script_root(data_dir: &str, automation_id: &str) -> String {
    let trimmed = data_dir.trim_end_matches(['/', '\\']);
    format!("{trimmed}/automation-scripts/{automation_id}")
}

/// Where a thread's storage directory lives under a host's data directory.
///
/// The layout is bb's, verbatim: `<data_dir>/thread-storage/<thread_id>`, and
/// it lives in this crate because it is the one thing the control plane and a
/// worker must agree on without either importing the other. The worker creates
/// and owns the directory; the control plane composes the same path from the
/// data directory the host *reported* at enrollment, so `storageRootPath` is a
/// real location on a real machine rather than a guess.
///
/// `data_dir` is a machine-local path string, not a `PathBuf`, because that is
/// how it crosses the wire.
pub fn thread_storage_root(data_dir: &str, thread_id: &str) -> String {
    // `/` separators rather than `Path::join`: a data directory reported by a
    // Windows worker is still a string the control plane must describe, and
    // every client is a browser that renders `/`.
    let trimmed = data_dir.trim_end_matches(['/', '\\']);
    format!("{trimmed}/thread-storage/{thread_id}")
}

/// Where a project's uploaded attachments live under a host's data directory.
///
/// The sibling of [`thread_storage_root`], for the same reason: the layout
/// belongs to the worker, and the control plane can only name it from the data
/// directory the host *reported*. `<data_dir>/project-attachments/<project_id>`
/// keeps every attachment under the machine that owns the project, so an
/// upload can never land on the server's own disk.
pub fn project_attachments_root(data_dir: &str, project_id: &str) -> String {
    let trimmed = data_dir.trim_end_matches(['/', '\\']);
    format!("{trimmed}/project-attachments/{project_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_domain::RunEvent;

    fn sample_dispatch() -> RunDispatch {
        RunDispatch {
            run_id: RunId::mint(),
            thread_id: ThreadId::mint(),
            project_id: ProjectId::mint(),
            host_id: HostId::mint(),
            prompt: "hello".into(),
            provider: ProviderSpec::pi(),
            permission_ceiling: HostPermissionMode::Full,
            deadline_ms: 12,
            created_at_ms: 1,
            provider_session_id: None,
            model: None,
            reasoning_level: None,
        }
    }

    #[test]
    fn a_dispatch_round_trips() {
        let dispatch = sample_dispatch();
        let encoded = serde_json::to_string(&dispatch).unwrap();
        assert_eq!(
            serde_json::from_str::<RunDispatch>(&encoded).unwrap(),
            dispatch
        );
    }

    /// The choices are additive: a control plane that predates them sends no
    /// such keys, and the worker reads that as the agent's own default.
    #[test]
    fn a_dispatch_from_an_older_control_plane_carries_no_choices() {
        let older = serde_json::to_value(sample_dispatch()).unwrap();
        assert!(older.as_object().unwrap().get("model").is_none());
        assert!(older.as_object().unwrap().get("reasoning_level").is_none());
        let decoded: RunDispatch = serde_json::from_value(older).unwrap();
        assert_eq!(decoded.model, None);
        assert_eq!(decoded.reasoning_level, None);
    }

    #[test]
    fn a_dispatch_carries_the_clients_model_and_reasoning_choices() {
        let dispatch = RunDispatch {
            model: Some("anthropic/claude-sonnet-4".into()),
            // A level bb's own schema does not name: the vocabulary is the
            // agent's, so the choice travels as the agent spelled it.
            reasoning_level: Some(ReasoningLevel::from("off")),
            ..sample_dispatch()
        };
        let encoded = serde_json::to_value(&dispatch).unwrap();
        assert_eq!(encoded["model"], "anthropic/claude-sonnet-4");
        assert_eq!(encoded["reasoning_level"], "off");
        assert_eq!(
            serde_json::from_value::<RunDispatch>(encoded).unwrap(),
            dispatch
        );
    }

    #[test]
    fn a_report_round_trips() {
        let report = ProviderReport {
            host_id: HostId::mint(),
            event: RunEvent::new(
                ThreadId::mint(),
                ProjectId::mint(),
                RunId::mint(),
                1,
                loom_domain::ProviderEvent::ProviderWarning {
                    provider_thread_id: "p".into(),
                    category: loom_domain::ProviderWarningCategory::General,
                    summary: Some("careful".into()),
                    details: None,
                },
            ),
        };
        let encoded = serde_json::to_string(&report).unwrap();
        assert_eq!(
            serde_json::from_str::<ProviderReport>(&encoded).unwrap(),
            report
        );
    }

    #[test]
    fn the_pi_spec_uses_embedded_acp() {
        let spec = ProviderSpec::pi();
        assert_eq!(spec.command, "pi");
        assert_eq!(spec.args, Vec::<String>::new());
        assert_eq!(spec.launch, ProviderLaunch::AcpEmbeddedPi);
    }

    #[test]
    fn a_custom_spec_is_an_acp_stdio_agent() {
        let spec = ProviderSpec::acp("agent", vec!["--x".into()]);
        assert_eq!(spec.launch, ProviderLaunch::AcpStdio);
    }

    #[test]
    fn an_environment_provision_round_trips_both_outcomes() {
        let provision = EnvironmentProvision {
            environment_id: EnvironmentId::mint(),
            project_id: ProjectId::mint(),
            host_id: HostId::mint(),
            created_at_ms: 7,
        };
        let encoded = serde_json::to_string(&provision).unwrap();
        assert_eq!(
            serde_json::from_str::<EnvironmentProvision>(&encoded).unwrap(),
            provision
        );

        let ok = EnvironmentProvisionReport {
            host_id: HostId::mint(),
            environment_id: EnvironmentId::mint(),
            outcome: EnvironmentProvisionOutcome::Provisioned {
                path: "/srv/loom".into(),
            },
        };
        let value = serde_json::to_value(&ok).unwrap();
        assert_eq!(value["outcome"]["outcome"], "provisioned");
        assert_eq!(
            serde_json::from_str::<EnvironmentProvisionReport>(
                &serde_json::to_string(&ok).unwrap()
            )
            .unwrap(),
            ok
        );

        let failed = EnvironmentProvisionReport {
            host_id: HostId::mint(),
            environment_id: EnvironmentId::mint(),
            outcome: EnvironmentProvisionOutcome::Failed {
                error: "permission denied".into(),
            },
        };
        let value = serde_json::to_value(&failed).unwrap();
        assert_eq!(value["outcome"]["outcome"], "failed");
        assert_eq!(value["outcome"]["error"], "permission denied");
    }

    #[test]
    fn an_interaction_request_round_trips() {
        let request = InteractionRequest {
            host_id: HostId::mint(),
            run_id: RunId::mint(),
            thread_id: ThreadId::mint(),
            project_id: ProjectId::mint(),
            request_id: "call-1".into(),
            provider_thread_id: Some("acp-1".into()),
            kind: loom_domain::InteractionKind::Approval,
            payload: loom_domain::InteractionPayload::new(
                loom_domain::InteractionKind::Approval,
                serde_json::json!({
                    "kind": "approval",
                    "subject": {
                        "kind": "tool_use",
                        "itemId": "call-1",
                        "tool": "other",
                        "presentation": { "label": { "pending": "Run", "completed": "Ran" }, "icon": { "glyph": "Terminal" } },
                    },
                    "reason": null,
                    "availableDecisions": ["allow_once", "allow_for_session", "deny"],
                }),
            ),
            expires_at_ms: Some(1),
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(
            serde_json::from_str::<InteractionRequest>(&encoded).unwrap(),
            request
        );
        // The worker identity survives the wire: the answer is matched on it.
        let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["request_id"], "call-1");
        assert_eq!(value["payload"]["kind"], "approval");
    }

    #[test]
    fn every_answer_variant_round_trips() {
        for answer in [
            InteractionAnswer::Decision {
                decision: PermissionDecision::AllowOnce,
            },
            InteractionAnswer::Decision {
                decision: PermissionDecision::AllowForSession,
            },
            InteractionAnswer::Decision {
                decision: PermissionDecision::Deny,
            },
            InteractionAnswer::Cancelled {
                reason: "no client answered".into(),
            },
        ] {
            let frame = InteractionResolutionFrame {
                host_id: HostId::mint(),
                run_id: RunId::mint(),
                thread_id: ThreadId::mint(),
                request_id: "call-1".into(),
                interaction_id: "intr_01M".into(),
                answer: answer.clone(),
                created_at_ms: 7,
            };
            let encoded = serde_json::to_string(&frame).unwrap();
            assert_eq!(
                serde_json::from_str::<InteractionResolutionFrame>(&encoded).unwrap(),
                frame
            );
        }
        // The tag is what the worker dispatches on, and a denial is not
        // representable as an allow.
        let denied = serde_json::to_value(InteractionAnswer::Decision {
            decision: PermissionDecision::Deny,
        })
        .unwrap();
        assert_eq!(denied["kind"], "decision");
        assert_eq!(denied["decision"], "deny");
    }

    #[test]
    fn a_finished_report_carries_the_outcome() {
        let report = ProviderReport {
            host_id: HostId::mint(),
            event: RunEvent::failed(
                ThreadId::mint(),
                ProjectId::mint(),
                RunId::mint(),
                1,
                loom_domain::TurnStatus::Failed,
                "exit 1",
            ),
        };
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["event"]["event"]["type"], "turn/completed");
        assert_eq!(value["event"]["event"]["status"], "failed");
        assert_eq!(value["event"]["event"]["error"]["message"], "exit 1");
    }
}

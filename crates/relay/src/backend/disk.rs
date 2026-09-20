//! Crash-safe, durable backend.
//!
//! This is the backend that makes replay survive a process restart. It stores
//! one append-only log file per shard under a data directory:
//!
//! ```text
//!   <root>/shard-0.log … <root>/shard-7.log
//! ```
//!
//! # Why a hand-rolled log instead of redb/sled
//!
//! `redb` and `sled` are excellent general-purpose embedded stores, and either
//! would work here. They also bring a large dependency tree, their own file
//! format, their own locking and their own background compactor. The relay
//! needs exactly one thing from storage — an append-ordered, replayable,
//! trimmable log — and the project's default is "in-process, zero external
//! service". A ~500-line append-only file log with an explicit framing format
//! gives that with **no new dependencies at all**, and makes the crash-safety
//! argument inspectable instead of delegated. The cost is that compaction and
//! recovery are ours to get right, which is why both have dedicated tests.
//!
//! # Shape
//!
//! Each shard owns its own mutex-protected in-memory view (the same shape as
//! [`super::memory::MemoryBackend`]) plus one dedicated writer thread. The
//! trait is synchronous and is called from the server's tokio readers, so the
//! split matters:
//!
//! * `append` / `trim` mutate the in-memory view and hand the writer a command
//!   over an unbounded channel — the reactor never touches a file;
//! * `read` / `len` are served from memory and never do IO;
//! * all `write`/`flush`/`sync`/compaction happen on the shard's writer thread.
//!
//! The command channel is unbounded so `append` can never stall a tokio task on
//! a slow disk. That is a deliberate trade: a writer that falls permanently
//! behind lets queued bytes grow, bounded only by how far behind it falls. In
//! steady state the queue holds a handful of frames, and the in-memory view is
//! independently capped at `max_len`. A writer that fails records a sticky
//! error instead of silently dropping records.
//!
//! Because each shard has its own mutex *and* its own writer thread, a slow
//! disk on shard 0 cannot stall shard 1.
//!
//! # Crash safety
//!
//! Every record is framed as:
//!
//! ```text
//!   magic u32 · version u8 · kind u8 · reserved u16
//!   body_len u32 · id_len u16 · origin_len u16 · payload_len u32
//!   event_id u128 · created_at_ms u64 · body_crc u32
//!   <id> <origin> <payload>            ← body_len bytes
//! ```
//!
//! Recovery scans forward and stops at the first byte that is not a complete,
//! length-consistent, CRC-valid record, then truncates the file there. A
//! process that dies mid-write therefore loses at most the unterminated tail;
//! it can never read half a record, because a half record fails both the
//! length check and the checksum.
//!
//! Appends are not `fsync`ed individually: `append` returning means "in the
//! log and on its way to disk", which is the right trade for a relay whose
//! consumers already replay and deduplicate. Call [`DiskBackend::flush`] (or
//! drop the backend) to make everything durable before a planned exit.
//!
//! # Retention and space reclamation
//!
//! `trim` and the per-shard cap only mark records dead; when enough of a
//! shard's file is dead (or on an explicit [`DiskBackend::compact`]) the writer
//! rewrites the live records to a temporary file and renames it over the
//! original, so trimmed bytes are actually returned to the filesystem.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use bytes::Bytes;

use crate::error::{RelayError, Result};
use crate::event_id::EventId;
use crate::scope::{Scope, ShardId, SHARD_COUNT};

use super::{LogRecord, RelayBackend};

/// Directory entry prefix for a shard log.
const FILE_PREFIX: &str = "shard-";
/// Directory entry suffix for a shard log.
const FILE_SUFFIX: &str = ".log";
/// Suffix used for the compaction rewrite before it is renamed into place.
const COMPACT_SUFFIX: &str = "compact";

/// Marks the start of a framed record.
const RECORD_MAGIC: u32 = 0x4C4F_4F4D; // "LOOM", little-endian
/// Framing version.
const RECORD_VERSION: u8 = 1;
/// Bytes of fixed header prefixing every record's body.
const PREFIX_LEN: usize = 48;
/// Upper bound on a single record body, so garbage lengths fail fast.
const MAX_BODY_LEN: u32 = 256 * 1024 * 1024;

/// Scope kind codes. These are part of the on-disk format and must stay
/// stable; `Scope::kind()` is the canonical text form they correspond to.
const KIND_GLOBAL: u8 = 0;
const KIND_PROJECT: u8 = 1;
const KIND_THREAD: u8 = 2;
const KIND_HOST: u8 = 3;
const KIND_CLIENT: u8 = 4;
const KIND_USER: u8 = 5;

/// A durable, sharded, append-only relay log.
///
/// A data directory must be owned by a single backend instance. Opening the
/// same root twice would have two processes appending to one file, which the
/// framing does not protect against.
pub struct DiskBackend {
    root: PathBuf,
    shards: Vec<Shard>,
    max_len: usize,
}

struct Shard {
    log: Mutex<Vec<LogRecord>>,
    writer: WriterHandle,
}

/// The caller-facing half of a shard writer.
struct WriterHandle {
    tx: Option<Sender<Command>>,
    handle: Option<JoinHandle<()>>,
    error: Arc<Mutex<Option<String>>>,
}

impl WriterHandle {
    fn sender(&self) -> Result<&Sender<Command>> {
        self.tx
            .as_ref()
            .ok_or_else(|| RelayError::backend("shard writer is stopped"))
    }

    /// Returns the first IO error the writer hit, if any.
    fn check(&self) -> Result<()> {
        let guard = self
            .error
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match guard.as_ref() {
            Some(message) => Err(RelayError::backend(message.clone())),
            None => Ok(()),
        }
    }
}

/// One live record's extent inside the shard's current file.
#[derive(Clone, Copy)]
struct Entry {
    start: u64,
    len: u64,
    created_at_ms: u64,
}

enum Command {
    Append { framed: Vec<u8>, created_at_ms: u64 },
    EvictOldest { count: usize },
    Trim { before_ms: u64 },
    Flush { ack: Sender<Result<()>> },
    Compact { ack: Sender<Result<()>> },
    Stop { ack: Sender<Result<()>> },
}

impl DiskBackend {
    /// Opens (or creates) the log under `root`, retaining at most `max_len`
    /// records per shard (minimum 1).
    pub fn open(root: impl AsRef<Path>, max_len: usize) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root).map_err(backend_io)?;
        let max_len = max_len.max(1);

        let mut shards = Vec::with_capacity(usize::from(SHARD_COUNT));
        for shard in 0..SHARD_COUNT {
            let path = shard_path(&root, shard);
            let (records, loaded) = load_shard(&path, max_len)?;

            let (tx, rx) = mpsc::channel();
            let error = Arc::new(Mutex::new(None));
            let writer_error = Arc::clone(&error);
            let writer_path = path.clone();
            let handle = thread::Builder::new()
                .name(format!("loom-relay-shard-{shard}"))
                .spawn(move || writer_loop(writer_path, loaded, rx, writer_error))
                .map_err(backend_io)?;

            shards.push(Shard {
                log: Mutex::new(records),
                writer: WriterHandle {
                    tx: Some(tx),
                    handle: Some(handle),
                    error,
                },
            });
        }

        Ok(Self {
            root,
            shards,
            max_len,
        })
    }

    /// The data directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file backing `shard`.
    pub fn shard_path(&self, shard: ShardId) -> PathBuf {
        shard_path(&self.root, shard)
    }

    /// Number of bytes the shard's file occupies right now.
    ///
    /// Exposed for space-reclamation tests and operator tooling.
    pub fn file_len(&self, shard: ShardId) -> Result<u64> {
        let path = self.shard_path(shard);
        let metadata = std::fs::metadata(&path).map_err(backend_io)?;
        Ok(metadata.len())
    }

    /// The configured per-shard cap.
    pub fn max_len(&self) -> usize {
        self.max_len
    }

    /// Durably flushes every shard and reports the first writer error.
    ///
    /// Waits for each shard's writer to have processed everything queued before
    /// this call, so it drains as well as syncs.
    pub fn flush_shards(&self) -> Result<()> {
        for shard in &self.shards {
            let (command, done) = Command::flush();
            wait_command(shard.writer.sender()?, command, done)?;
            shard.writer.check()?;
        }
        Ok(())
    }

    /// Forces a compaction pass on every shard, returning space to the
    /// filesystem. Normal operation compacts automatically once a file is
    /// mostly dead; this exists for tests and for "reclaim now" operators.
    pub fn compact(&self) -> Result<()> {
        for shard in &self.shards {
            let (command, done) = Command::compact();
            wait_command(shard.writer.sender()?, command, done)?;
            shard.writer.check()?;
        }
        Ok(())
    }

    fn shard(&self, shard: ShardId) -> Result<&Shard> {
        self.shards
            .get(usize::from(shard))
            .ok_or_else(|| RelayError::backend(shard_out_of_range(shard)))
    }
}

impl RelayBackend for DiskBackend {
    fn shard_count(&self) -> u8 {
        SHARD_COUNT
    }

    fn append(&self, shard: ShardId, record: LogRecord) -> Result<()> {
        let shard = self.shard(shard)?;
        shard.writer.check()?;

        let framed = encode(&record)?;
        let created_at_ms = record.created_at_ms;

        let mut log = shard
            .log
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        log.push(record);
        let overflow = log.len().saturating_sub(self.max_len);
        if overflow > 0 {
            log.drain(0..overflow);
        }
        drop(log);

        // The writer applies the same mutations in the same order, so its live
        // set tracks the in-memory one exactly. Evicting first keeps a
        // compaction from rewriting a record that is about to die.
        let tx = shard.writer.sender()?;
        if overflow > 0 {
            tx.send(Command::EvictOldest { count: overflow })
                .map_err(backend_io)?;
        }
        tx.send(Command::Append {
            framed,
            created_at_ms,
        })
        .map_err(backend_io)?;
        Ok(())
    }

    fn read_after(
        &self,
        shard: ShardId,
        after: Option<EventId>,
        limit: usize,
    ) -> Result<Vec<LogRecord>> {
        Ok(self
            .shard(shard)?
            .log
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .filter(|record| match after {
                None => true,
                Some(cursor) => record.event_id > cursor,
            })
            .take(limit)
            .cloned()
            .collect())
    }

    fn trim(&self, shard: ShardId, before_ms: u64) -> Result<u64> {
        let shard = self.shard(shard)?;
        let removed = {
            let mut log = shard
                .log
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let before = log.len();
            log.retain(|record| record.created_at_ms >= before_ms);
            (before - log.len()) as u64
        };

        // Even with nothing removed the command is harmless, and forwarding it
        // keeps the writer's view of time in step with the in-memory one.
        shard
            .writer
            .sender()?
            .send(Command::Trim { before_ms })
            .map_err(backend_io)?;
        Ok(removed)
    }

    fn len(&self, shard: ShardId) -> Result<usize> {
        Ok(self
            .shard(shard)?
            .log
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len())
    }

    /// The first write failure any shard's writer latched.
    ///
    /// `append` publishes its write to the shard's writer thread and returns,
    /// so the failure of *that* write is only known afterwards. It is latched
    /// rather than dropped, and reported here, so a broken disk is observable
    /// instead of looking like a healthy server whose log happens to be
    /// memory-only.
    fn backend_error(&self) -> Option<String> {
        self.shards.iter().find_map(|shard| {
            shard
                .writer
                .error
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone()
        })
    }

    /// Drains and syncs: see [`DiskBackend::flush_shards`].
    fn flush(&self) -> Result<()> {
        self.flush_shards()
    }
}

impl Drop for DiskBackend {
    fn drop(&mut self) {
        for shard in &mut self.shards {
            if let Some(tx) = shard.writer.tx.take() {
                let (ack, done) = mpsc::channel();
                if tx.send(Command::Stop { ack }).is_ok() {
                    let _ = done.recv();
                }
            }
            if let Some(handle) = shard.writer.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

impl Command {
    fn flush() -> (Self, Receiver<Result<()>>) {
        let (ack, done) = mpsc::channel();
        (Command::Flush { ack }, done)
    }

    fn compact() -> (Self, Receiver<Result<()>>) {
        let (ack, done) = mpsc::channel();
        (Command::Compact { ack }, done)
    }
}

/// Sends `command` and blocks for its acknowledgement.
///
/// Only used by the explicit `flush`/`compact` entry points, which are not on
/// the relay's hot path.
fn wait_command(tx: &Sender<Command>, command: Command, done: Receiver<Result<()>>) -> Result<()> {
    tx.send(command).map_err(backend_io)?;
    done.recv().map_err(backend_io)?
}

fn shard_path(root: &Path, shard: ShardId) -> PathBuf {
    root.join(format!("{FILE_PREFIX}{shard}{FILE_SUFFIX}"))
}

fn shard_out_of_range(shard: ShardId) -> String {
    format!("shard {shard} out of range (0..{SHARD_COUNT})")
}

fn backend_io(error: impl std::fmt::Display) -> RelayError {
    RelayError::backend(error.to_string())
}

/// Everything the shard writer owns: the file, its live extents, and the
/// bookkeeping compaction needs.
struct WriterState {
    path: PathBuf,
    file: File,
    entries: Vec<Entry>,
    file_len: u64,
    dead_bytes: u64,
    error: Arc<Mutex<Option<String>>>,
}

impl WriterState {
    fn append(&mut self, framed: &[u8], created_at_ms: u64) -> Result<()> {
        self.file.write_all(framed)?;
        self.entries.push(Entry {
            start: self.file_len,
            len: framed.len() as u64,
            created_at_ms,
        });
        self.file_len += framed.len() as u64;
        self.maybe_compact()
    }

    fn evict_oldest(&mut self, count: usize) {
        let count = count.min(self.entries.len());
        let dead: u64 = self.entries.drain(0..count).map(|entry| entry.len).sum();
        self.dead_bytes += dead;
        if let Err(error) = self.maybe_compact() {
            self.record(error);
        }
    }

    fn trim(&mut self, before_ms: u64) -> Result<()> {
        let mut dead = 0u64;
        self.entries.retain(|entry| {
            if entry.created_at_ms < before_ms {
                dead += entry.len;
                false
            } else {
                true
            }
        });
        self.dead_bytes += dead;
        self.maybe_compact()
    }

    fn flush(&mut self) -> Result<()> {
        self.file.flush()?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Compacts once at least half the file is dead. Below that the rewrite
    /// would cost more than the space it returns.
    fn maybe_compact(&mut self) -> Result<()> {
        if self.dead_bytes > 0 && self.dead_bytes.saturating_mul(2) >= self.file_len {
            self.compact()?;
        }
        Ok(())
    }

    fn compact(&mut self) -> Result<()> {
        if self.entries.is_empty() {
            self.file.set_len(0)?;
            self.file.seek(SeekFrom::Start(0))?;
            self.file_len = 0;
            self.dead_bytes = 0;
            return Ok(());
        }

        let temp = self.path.with_extension(COMPACT_SUFFIX);
        let mut rewritten = File::create(&temp)?;
        let mut new_len = 0u64;
        let mut buffer = Vec::new();
        for entry in &mut self.entries {
            buffer.clear();
            buffer.resize(entry.len as usize, 0);
            self.file.seek(SeekFrom::Start(entry.start))?;
            self.file.read_exact(&mut buffer)?;
            rewritten.write_all(&buffer)?;
            entry.start = new_len;
            new_len += entry.len;
        }
        rewritten.sync_all()?;
        drop(rewritten);
        std::fs::rename(&temp, &self.path)?;

        self.file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        self.file.seek(SeekFrom::End(0))?;
        self.file_len = new_len;
        self.dead_bytes = 0;
        Ok(())
    }

    fn record(&self, error: impl std::fmt::Display) {
        let mut guard = self
            .error
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if guard.is_none() {
            *guard = Some(error.to_string());
        }
    }
}

/// The shard's IO thread. Every filesystem call for this shard happens here,
/// never on a caller's task.
fn writer_loop(
    path: PathBuf,
    loaded: ShardLoad,
    rx: Receiver<Command>,
    error: Arc<Mutex<Option<String>>>,
) {
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(open_error) => {
            let mut guard = error.lock().unwrap_or_else(|poison| poison.into_inner());
            if guard.is_none() {
                *guard = Some(open_error.to_string());
            }
            // Drain so senders see a closed channel instead of buffering
            // commands that can never run.
            while rx.recv().is_ok() {}
            return;
        }
    };

    let mut state = WriterState {
        path,
        file,
        entries: loaded.entries,
        file_len: loaded.file_len,
        dead_bytes: loaded.dead_bytes,
        error,
    };

    while let Ok(command) = rx.recv() {
        match command {
            Command::Append {
                framed,
                created_at_ms,
            } => {
                if let Err(append_error) = state.append(&framed, created_at_ms) {
                    state.record(append_error);
                }
            }
            Command::EvictOldest { count } => state.evict_oldest(count),
            Command::Trim { before_ms } => {
                if let Err(trim_error) = state.trim(before_ms) {
                    state.record(trim_error);
                }
            }
            Command::Flush { ack } => {
                let result = state.flush();
                let _ = ack.send(result);
            }
            Command::Compact { ack } => {
                let result = state.compact().and_then(|()| state.flush());
                let _ = ack.send(result);
            }
            Command::Stop { ack } => {
                let result = state.flush();
                let _ = ack.send(result);
                break;
            }
        }
    }
}

/// What recovery hands to the writer thread.
struct ShardLoad {
    entries: Vec<Entry>,
    file_len: u64,
    dead_bytes: u64,
}

/// Scans a shard file, truncating any unreadable tail and dropping records
/// beyond `max_len`.
///
/// Returns the surviving records (the in-memory view) and what the writer
/// thread needs to keep appending to the same file.
#[allow(clippy::type_complexity)]
fn load_shard(path: &Path, max_len: usize) -> Result<(Vec<LogRecord>, ShardLoad)> {
    let scan = scan_file(path)?;
    let keep_from = scan.records.len().saturating_sub(max_len);
    let dead_bytes: u64 = scan.records[..keep_from]
        .iter()
        .map(|located| located.len)
        .sum();

    let entries = scan.records[keep_from..]
        .iter()
        .map(|located| Entry {
            start: located.start,
            len: located.len,
            created_at_ms: located.record.created_at_ms,
        })
        .collect();
    let records = scan.records[keep_from..]
        .iter()
        .map(|located| located.record.clone())
        .collect();

    Ok((
        records,
        ShardLoad {
            entries,
            file_len: scan.valid_len,
            dead_bytes,
        },
    ))
}

/// A record plus where it lives in the file.
struct Located {
    record: LogRecord,
    start: u64,
    len: u64,
}

struct Scan {
    records: Vec<Located>,
    /// Length of the valid, complete prefix of the file.
    valid_len: u64,
}

/// Walks a shard file from the start, stopping at the first unreadable record.
fn scan_file(path: &Path) -> Result<Scan> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(backend_io)?;
    let total = file.metadata().map_err(backend_io)?.len();

    let mut records = Vec::new();
    let mut offset = 0u64;
    while offset < total {
        match read_record_at(&mut file, offset) {
            Ok(Some((record, len))) => {
                records.push(Located {
                    record,
                    start: offset,
                    len,
                });
                offset += len;
            }
            // A corrupt or half-written record ends the valid prefix. Keep
            // everything before it and discard the rest.
            Ok(None) => break,
            Err(read_error) => return Err(backend_io(read_error)),
        }
    }

    if offset < total {
        file.set_len(offset).map_err(backend_io)?;
        file.sync_all().map_err(backend_io)?;
    }

    Ok(Scan {
        records,
        valid_len: offset,
    })
}

/// Reads one framed record at `offset`.
///
/// `Ok(None)` means "not a valid record here" — either end of file or a torn
/// tail; the caller stops scanning. `Err` is a real IO failure.
fn read_record_at(file: &mut File, offset: u64) -> io::Result<Option<(LogRecord, u64)>> {
    file.seek(SeekFrom::Start(offset))?;

    let mut prefix = [0u8; PREFIX_LEN];
    if !read_exact_or_eof(file, &mut prefix)? {
        return Ok(None);
    }

    let magic = u32::from_le_bytes(prefix[0..4].try_into().expect("4 bytes"));
    if magic != RECORD_MAGIC || prefix[4] != RECORD_VERSION {
        return Ok(None);
    }
    let kind = prefix[5];
    let body_len = read_u32(&prefix[8..12]) as usize;
    let id_len = read_u16(&prefix[12..14]) as usize;
    let origin_len = read_u16(&prefix[14..16]) as usize;
    let payload_len = read_u32(&prefix[16..20]) as usize;
    let event_id = read_u128(&prefix[20..36]);
    let created_at_ms = read_u64(&prefix[36..44]);
    let stored_crc = read_u32(&prefix[44..48]);

    if body_len > MAX_BODY_LEN as usize
        || id_len
            .checked_add(origin_len)
            .and_then(|sum| sum.checked_add(payload_len))
            != Some(body_len)
    {
        return Ok(None);
    }

    let mut body = vec![0u8; body_len];
    if !read_exact_or_eof(file, &mut body)? {
        return Ok(None);
    }
    if crc32(&body) != stored_crc {
        return Ok(None);
    }

    let id = match std::str::from_utf8(&body[..id_len]) {
        Ok(id) => id.to_string(),
        Err(_) => return Ok(None),
    };
    let origin = match std::str::from_utf8(&body[id_len..id_len + origin_len]) {
        Ok(origin) => origin.to_string(),
        Err(_) => return Ok(None),
    };
    let Some(scope) = scope_from_parts(kind, id) else {
        return Ok(None);
    };
    let payload = Bytes::copy_from_slice(&body[id_len + origin_len..]);

    let record = LogRecord {
        event_id: EventId::from_raw(event_id),
        scope,
        payload,
        created_at_ms,
        origin,
    };
    Ok(Some((record, (PREFIX_LEN + body_len) as u64)))
}

/// Fills `buf`, returning `false` on a short read (a torn tail) rather than an
/// error.
fn read_exact_or_eof(file: &mut File, buf: &mut [u8]) -> io::Result<bool> {
    match file.read_exact(buf) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error),
    }
}

fn encode(record: &LogRecord) -> Result<Vec<u8>> {
    let id = record.scope.id().as_bytes();
    let origin = record.origin.as_bytes();
    let payload = &record.payload[..];
    let body_len = id.len() + origin.len() + payload.len();
    if id.len() > usize::from(u16::MAX)
        || origin.len() > usize::from(u16::MAX)
        || body_len > MAX_BODY_LEN as usize
    {
        return Err(RelayError::backend(format!(
            "record too large to persist: {body_len} body bytes"
        )));
    }

    let mut framed = Vec::with_capacity(PREFIX_LEN + body_len);
    framed.extend_from_slice(&RECORD_MAGIC.to_le_bytes());
    framed.push(RECORD_VERSION);
    framed.push(kind_code(&record.scope));
    framed.extend_from_slice(&0u16.to_le_bytes());
    framed.extend_from_slice(&(body_len as u32).to_le_bytes());
    framed.extend_from_slice(&(id.len() as u16).to_le_bytes());
    framed.extend_from_slice(&(origin.len() as u16).to_le_bytes());
    framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    framed.extend_from_slice(&record.event_id.as_u128().to_le_bytes());
    framed.extend_from_slice(&record.created_at_ms.to_le_bytes());
    framed.extend_from_slice(&crc32_parts(&[id, origin, payload]).to_le_bytes());
    framed.extend_from_slice(id);
    framed.extend_from_slice(origin);
    framed.extend_from_slice(payload);
    Ok(framed)
}

fn kind_code(scope: &Scope) -> u8 {
    match scope {
        Scope::Global => KIND_GLOBAL,
        Scope::Project(_) => KIND_PROJECT,
        Scope::Thread(_) => KIND_THREAD,
        Scope::Host(_) => KIND_HOST,
        Scope::Client(_) => KIND_CLIENT,
        Scope::User(_) => KIND_USER,
    }
}

fn scope_from_parts(code: u8, id: String) -> Option<Scope> {
    match code {
        KIND_GLOBAL => Some(Scope::Global),
        KIND_PROJECT => Some(Scope::Project(id)),
        KIND_THREAD => Some(Scope::Thread(id)),
        KIND_HOST => Some(Scope::Host(id)),
        KIND_CLIENT => Some(Scope::Client(id)),
        KIND_USER => Some(Scope::User(id)),
        _ => None,
    }
}

fn read_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes.try_into().expect("2 bytes"))
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("4 bytes"))
}

fn read_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("8 bytes"))
}

fn read_u128(bytes: &[u8]) -> u128 {
    u128::from_le_bytes(bytes.try_into().expect("16 bytes"))
}

fn crc32(bytes: &[u8]) -> u32 {
    crc32_parts(&[bytes])
}

/// CRC-32/ISO-HDLC, hand-rolled so persistence needs no dependency.
fn crc32_parts(parts: &[&[u8]]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for part in parts {
        for &byte in *part {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::{memory_backend, Relay};
    use crate::retention::Retention;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn record(created_at_ms: u64) -> LogRecord {
        LogRecord {
            event_id: EventId::new(),
            scope: Scope::Thread("thr_1".into()),
            payload: Bytes::from_static(b"{}"),
            created_at_ms,
            origin: "node-a".into(),
        }
    }

    /// A flush covers the writes queued before it, not merely the ones that
    /// happened to have finished.
    ///
    /// This is what a server shutdown stands on: it appends, closes the relay so
    /// nothing more can be accepted, and flushes. If the flush only synced what
    /// the writer had already done, the record below — large enough that the
    /// writer cannot possibly have finished it by the next line — would be a
    /// half-written tail, and reopening the directory would read a shorter log
    /// (or a torn one).
    #[test]
    fn a_flush_covers_the_writes_queued_before_it() {
        let dir = TempDir::new().unwrap();
        let backend = DiskBackend::open(dir.path(), 8).unwrap();
        let mut large = record(10);
        large.payload = Bytes::from(vec![b'x'; 16 * 1024 * 1024]);
        backend.append(3, large).unwrap();

        backend.flush_shards().unwrap();

        let reopened = DiskBackend::open(dir.path(), 8).unwrap();
        let read = reopened.read_after(3, None, 8).unwrap();
        assert_eq!(read.len(), 1, "the queued record must be on disk");
        assert_eq!(read[0].created_at_ms, 10);
        assert_eq!(read[0].payload.len(), 16 * 1024 * 1024);
    }

    #[test]
    fn append_and_read_in_order() {
        let dir = TempDir::new().unwrap();
        let backend = DiskBackend::open(dir.path(), 100).unwrap();
        for ts in [10, 20, 30] {
            backend.append(0, record(ts)).unwrap();
        }
        let read = backend.read_after(0, None, 100).unwrap();
        let stamps: Vec<u64> = read.iter().map(|record| record.created_at_ms).collect();
        assert_eq!(stamps, vec![10, 20, 30]);
        backend.flush_shards().unwrap();
    }

    #[test]
    fn read_after_returns_only_newer_records() {
        let dir = TempDir::new().unwrap();
        let backend = DiskBackend::open(dir.path(), 100).unwrap();
        let mut ids = Vec::new();
        for ts in [10, 20, 30, 40] {
            let entry = record(ts);
            ids.push(entry.event_id);
            backend.append(1, entry).unwrap();
        }

        let all = backend.read_after(1, None, 100).unwrap();
        assert_eq!(all.len(), 4);

        // The cursor is exclusive, so resuming after the second record yields
        // the last two — in append order, regardless of timestamp.
        let resumed = backend.read_after(1, Some(ids[1]), 100).unwrap();
        let stamps: Vec<u64> = resumed.iter().map(|r| r.created_at_ms).collect();
        assert_eq!(stamps, vec![30, 40]);

        let limited = backend.read_after(1, None, 2).unwrap();
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].created_at_ms, 10);
        backend.flush_shards().unwrap();
    }

    /// A burst larger than the read limit, all in one millisecond, must still
    /// make progress rather than pinning the reader on records it passed.
    #[test]
    fn read_after_progresses_through_a_same_millisecond_burst() {
        let dir = TempDir::new().unwrap();
        let backend = DiskBackend::open(dir.path(), 1_000).unwrap();
        for _ in 0..32 {
            backend.append(1, record(50)).unwrap();
        }

        let mut cursor = None;
        let mut delivered = 0;
        loop {
            let batch = backend.read_after(1, cursor, 4).unwrap();
            if batch.is_empty() {
                break;
            }
            delivered += batch.len();
            cursor = Some(batch.last().unwrap().event_id);
        }
        assert_eq!(delivered, 32);
        backend.flush_shards().unwrap();
    }

    #[test]
    fn trim_removes_only_older_records() {
        let dir = TempDir::new().unwrap();
        let backend = DiskBackend::open(dir.path(), 100).unwrap();
        for ts in [10, 20, 30, 40] {
            backend.append(2, record(ts)).unwrap();
        }
        let removed = backend.trim(2, 30).unwrap();
        assert_eq!(removed, 2);
        let remaining: Vec<u64> = backend
            .read_after(2, None, 100)
            .unwrap()
            .iter()
            .map(|record| record.created_at_ms)
            .collect();
        assert_eq!(remaining, vec![30, 40]);
        backend.flush_shards().unwrap();
    }

    #[test]
    fn max_len_drops_the_oldest() {
        let dir = TempDir::new().unwrap();
        let backend = DiskBackend::open(dir.path(), 3).unwrap();
        for ts in [10, 20, 30, 40, 50] {
            backend.append(3, record(ts)).unwrap();
        }
        assert_eq!(backend.len(3).unwrap(), 3);
        let stamps: Vec<u64> = backend
            .read_after(3, None, 100)
            .unwrap()
            .iter()
            .map(|record| record.created_at_ms)
            .collect();
        assert_eq!(stamps, vec![30, 40, 50]);
        backend.flush_shards().unwrap();
    }

    #[test]
    fn shards_get_their_own_files_and_do_not_interfere() {
        let dir = TempDir::new().unwrap();
        let backend = DiskBackend::open(dir.path(), 100).unwrap();
        backend.append(0, record(10)).unwrap();
        backend.append(1, record(20)).unwrap();
        assert_eq!(backend.len(0).unwrap(), 1);
        assert_eq!(backend.len(1).unwrap(), 1);
        assert!(backend.shard_path(0).exists());
        assert!(backend.shard_path(1).exists());
        backend.trim(0, u64::MAX).unwrap();
        assert_eq!(backend.len(0).unwrap(), 0);
        assert_eq!(backend.len(1).unwrap(), 1);
        backend.flush_shards().unwrap();
    }

    #[test]
    fn out_of_range_shard_is_an_error() {
        let dir = TempDir::new().unwrap();
        let backend = DiskBackend::open(dir.path(), 10).unwrap();
        assert!(backend.append(SHARD_COUNT, record(1)).is_err());
        assert!(backend.read_after(SHARD_COUNT, None, 10).is_err());
    }

    /// An unwritable shard is refused at open, not silently degraded.
    ///
    /// Recovery scans each shard file read-write, so a data directory that
    /// cannot be written fails the backend at startup rather than producing a
    /// process that accepts events it cannot keep.
    #[cfg(unix)]
    #[test]
    fn opening_an_unwritable_shard_fails_rather_than_degrading_silently() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let unwritable = shard_path(dir.path(), 1);
        std::fs::write(&unwritable, b"").unwrap();
        std::fs::set_permissions(&unwritable, std::fs::Permissions::from_mode(0o444)).unwrap();

        // Skip when permissions are not enforced (a root shell).
        if OpenOptions::new().write(true).open(&unwritable).is_ok() {
            eprintln!("skipping: file permissions are not enforced for this user");
            return;
        }

        assert!(
            DiskBackend::open(dir.path(), 100).is_err(),
            "an unwritable shard must fail the backend at open"
        );
    }

    /// A failure that happens *after* a successful open — disk full, an IO
    /// error, the file replaced underneath the writer — lands in the shard's
    /// writer thread after the `append` that caused it already returned. It is
    /// latched and surfaced through `backend_error`, because the alternative is
    /// a process that serves reads from memory while its durability is gone,
    /// and whose loss only appears after a restart.
    ///
    /// The latch is poked directly here: an ENOSPC/IO error cannot be induced
    /// portably, and what needs pinning is that a latched failure is surfaced
    /// and that appends on that shard become loud.
    #[test]
    fn backend_error_surfaces_a_latched_writer_failure() {
        let dir = TempDir::new().unwrap();
        let backend = DiskBackend::open(dir.path(), 100).unwrap();
        assert_eq!(backend.backend_error(), None, "a fresh backend is healthy");

        // This is exactly what `WriterState::record` does on an IO failure.
        backend.shards[2]
            .writer
            .error
            .lock()
            .unwrap()
            .replace("disk on fire".to_owned());

        assert_eq!(
            backend.backend_error().as_deref(),
            Some("disk on fire"),
            "the latched failure must be observable"
        );

        // Appending to the broken shard is loud; other shards keep working, and
        // reads stay available so the server degrades rather than dies.
        assert!(backend.append(2, record(1)).is_err());
        backend.append(3, record(2)).unwrap();
        assert_eq!(backend.read_after(3, None, 10).unwrap().len(), 1);
    }

    #[test]
    fn records_survive_a_close_and_reopen() {
        let dir = TempDir::new().unwrap();
        let mut ids = Vec::new();
        {
            let backend = Arc::new(DiskBackend::open(dir.path(), 100).unwrap());
            let relay = Relay::new(backend, Retention::default(), "node-a").unwrap();
            for i in 0..5 {
                ids.push(
                    relay
                        .publish(Scope::Thread("thr_1".into()), format!("{{\"n\":{i}}}"))
                        .unwrap()
                        .event_id,
                );
            }
        }

        // Fresh process, same directory.
        let backend = Arc::new(DiskBackend::open(dir.path(), 100).unwrap());
        let relay = Relay::new(backend, Retention::default(), "node-a").unwrap();
        let replayed = relay
            .replay_scope(&Scope::Thread("thr_1".into()), 10)
            .unwrap();
        assert_eq!(replayed.len(), 5);
        let restored: Vec<EventId> = replayed.iter().map(|envelope| envelope.event_id).collect();
        assert_eq!(restored, ids);
    }

    #[test]
    fn every_scope_kind_round_trips() {
        let dir = TempDir::new().unwrap();
        let scopes = [
            Scope::Global,
            Scope::Project("prj_1".into()),
            Scope::Thread("thr_1".into()),
            Scope::Host("hst_1".into()),
            Scope::Client("cli_1".into()),
            Scope::User("usr_1".into()),
            Scope::Thread("线程-🦀".into()),
        ];
        let mut expected = Vec::new();
        {
            let backend = Arc::new(DiskBackend::open(dir.path(), 100).unwrap());
            for (i, scope) in scopes.iter().enumerate() {
                let record = LogRecord {
                    event_id: EventId::new(),
                    scope: scope.clone(),
                    payload: Bytes::from(format!("{{\"i\":{i}}}")),
                    created_at_ms: 1_000 + i as u64,
                    origin: "node-a".into(),
                };
                expected.push(record.clone());
                backend.append(scope.shard(), record).unwrap();
            }
            backend.flush_shards().unwrap();
        }

        let backend = DiskBackend::open(dir.path(), 100).unwrap();
        for record in expected {
            let read = backend
                .read_after(record.scope.shard(), None, 1_000)
                .unwrap();
            assert!(
                read.iter().any(|candidate| candidate == &record),
                "scope {} did not survive the round trip",
                record.scope
            );
        }
    }

    #[test]
    fn a_torn_tail_is_discarded_not_read() {
        let dir = TempDir::new().unwrap();
        {
            let backend = DiskBackend::open(dir.path(), 100).unwrap();
            for ts in [10, 20, 30] {
                backend.append(4, record(ts)).unwrap();
            }
            backend.flush_shards().unwrap();
        }

        let path = dir.path().join("shard-4.log");
        let good_len = std::fs::metadata(&path).unwrap().len();

        // Simulate a process dying mid-append: a valid-looking prefix that
        // stops half way through the record.
        {
            use std::io::Write as _;
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            let torn = encode(&record(40)).unwrap();
            file.write_all(&torn[..PREFIX_LEN + 3]).unwrap();
            file.sync_all().unwrap();
        }
        assert!(std::fs::metadata(&path).unwrap().len() > good_len);

        let backend = DiskBackend::open(dir.path(), 100).unwrap();
        let stamps: Vec<u64> = backend
            .read_after(4, None, 100)
            .unwrap()
            .iter()
            .map(|record| record.created_at_ms)
            .collect();
        assert_eq!(stamps, vec![10, 20, 30]);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), good_len);
    }

    #[test]
    fn a_corrupt_record_body_is_not_trusted() {
        let dir = TempDir::new().unwrap();
        {
            let backend = DiskBackend::open(dir.path(), 100).unwrap();
            for ts in [10, 20, 30] {
                backend.append(5, record(ts)).unwrap();
            }
            backend.flush_shards().unwrap();
        }

        let path = dir.path().join("shard-5.log");
        // Flip a byte inside the first record's payload.
        {
            use std::io::{Seek, SeekFrom, Write as _};
            let mut file = OpenOptions::new().write(true).open(&path).unwrap();
            file.seek(SeekFrom::Start(PREFIX_LEN as u64)).unwrap();
            file.write_all(b"X").unwrap();
            file.sync_all().unwrap();
        }

        let backend = DiskBackend::open(dir.path(), 100).unwrap();
        // The corrupt prefix is dropped, along with everything after it.
        assert_eq!(backend.len(5).unwrap(), 0);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    }

    #[test]
    fn trim_and_compaction_return_space_to_the_filesystem() {
        let dir = TempDir::new().unwrap();
        let backend = DiskBackend::open(dir.path(), 1_000).unwrap();
        for ts in 0..64 {
            backend.append(6, record(ts)).unwrap();
        }
        backend.flush_shards().unwrap();
        let before = backend.file_len(6).unwrap();
        assert!(before > 0);

        let removed = backend.trim(6, 64).unwrap();
        assert_eq!(removed, 64);
        backend.flush_shards().unwrap();

        let after = backend.file_len(6).unwrap();
        assert_eq!(
            after, 0,
            "trim must reclaim the file, got {after} of {before}"
        );
    }

    #[test]
    fn the_per_shard_cap_also_reclaims_space() {
        let dir = TempDir::new().unwrap();
        let backend = DiskBackend::open(dir.path(), 8).unwrap();
        for ts in 0..400 {
            backend.append(7, record(ts)).unwrap();
        }
        backend.flush_shards().unwrap();
        // 400 records at the cap of 8 must not leave 400 records' worth of
        // dead bytes behind.
        let full = encode(&record(0)).unwrap().len() as u64;
        assert!(backend.file_len(7).unwrap() < 8 * full * 4);
    }

    #[test]
    fn reopening_applies_the_current_cap() {
        let dir = TempDir::new().unwrap();
        {
            let backend = DiskBackend::open(dir.path(), 100).unwrap();
            for ts in 0..50 {
                backend.append(0, record(ts)).unwrap();
            }
            backend.flush_shards().unwrap();
        }
        let backend = DiskBackend::open(dir.path(), 5).unwrap();
        assert_eq!(backend.len(0).unwrap(), 5);
        let stamps: Vec<u64> = backend
            .read_after(0, None, 100)
            .unwrap()
            .iter()
            .map(|record| record.created_at_ms)
            .collect();
        assert_eq!(stamps, vec![45, 46, 47, 48, 49]);
        backend.flush_shards().unwrap();
    }

    #[test]
    fn a_disk_backend_plugs_into_the_relay_unchanged() {
        let dir = TempDir::new().unwrap();
        let backend: crate::backend::SharedBackend =
            Arc::new(DiskBackend::open(dir.path(), 100).unwrap());
        let relay = Relay::new(backend, Retention::default(), "node-a").unwrap();
        assert_eq!(relay.backend().shard_count(), SHARD_COUNT);
        let _memory = memory_backend(10);
    }
}

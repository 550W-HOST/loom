//! Shared relay log on Redis Streams.
//!
//! The in-process and disk backends give a server everything except one
//! property: a restart is invisible only if the log outlives the process *and*
//! a second node can attach to it. This backend is that shared log. It is the
//! one that makes a rolling upgrade of `loom-server` not disconnect connected
//! workers, because a fresh server process reads the same window a dead one
//! was reading.
//!
//! # Shape
//!
//! One Redis Stream per relay shard:
//!
//! ```text
//!   <prefix>:shard:0 … <prefix>:shard:7
//! ```
//!
//! A record is one stream entry whose fields are its [`LogRecord`] parts
//! (`e` event id, `t` created-at, `o` origin, `k` scope kind, `i` scope id,
//! `p` payload). The stream entry id is Redis's own and is never used for
//! routing: [`crate::scope::shard_for`] remains the only routing function, so
//! every implementation of the relay — Rust or not — agrees on where a scope
//! lives.
//!
//! `append` is `XADD key MAXLEN = <max_len> * ...`. The *exact* `MAXLEN` (not
//! the approximate `~` form) keeps the per-shard cap a hard bound, which is
//! what the contract with the other backends promises. `read` is one `XRANGE`
//! filtered by `created_at_ms`, and `trim` deletes the entries older than the
//! cut-off, so the three retained horizons keep their meaning unchanged.
//!
//! # Trade-off: a synchronous client
//!
//! [`RelayBackend`] is synchronous and the disk backend keeps it non-blocking
//! with a per-shard writer thread and an in-memory view. This backend does the
//! simpler thing: every call is one Redis round trip on a per-shard connection.
//! That keeps behaviour identical to the in-process backend — a read always
//! sees the source of truth, including another node's writes, with no
//! replication lag and no second consistency model — at the cost of blocking
//! the caller for the round trip. Redis is expected to run on the server's
//! machine or its LAN (see `docs/redis-backend.md`); the read/write timeout is
//! the bound on how long a stalled Redis can hold a caller.
//!
//! # Connections and reconnection
//!
//! Connections are per shard, created lazily, and hold one mutex each, so
//! shards never contend. If an operation fails on IO, the connection is
//! discarded, a fresh one is opened, and the operation is attempted once more:
//! a Redis restart, a connection reset or an idle-timeout kill is transparent
//! to the relay above. A failure that repeats is surfaced.
//!
//! # Deliberately not supported
//!
//! TLS (`rediss://`), RESP3, cluster redirects, sentinel and pipelining. Each
//! is a dependency or a state machine this layer does not need; the deployment
//! requirements section explains what to do instead.

mod resp;

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use bytes::Bytes;

use crate::error::{RelayError, Result};
use crate::event_id::EventId;
use crate::scope::{Scope, ShardId, SHARD_COUNT};

use self::resp::{Connection, Value};
use super::{LogRecord, RelayBackend};

/// Field name carrying the event id inside a stream entry.
const FIELD_EVENT_ID: &[u8] = b"e";
/// Field name carrying the producer timestamp.
const FIELD_CREATED_AT: &[u8] = b"t";
/// Field name carrying the producing node.
const FIELD_ORIGIN: &[u8] = b"o";
/// Field name carrying the scope kind.
const FIELD_SCOPE_KIND: &[u8] = b"k";
/// Field name carrying the scope id.
const FIELD_SCOPE_ID: &[u8] = b"i";
/// Field name carrying the payload bytes.
const FIELD_PAYLOAD: &[u8] = b"p";

/// How many stream ids to pass to one `XDEL`.
const TRIM_BATCH: usize = 256;

/// How to reach the shared Redis and where in it the log lives.
///
/// `key_prefix` is the only collision knob: two deployments that share a Redis
/// must use different prefixes, or they will merge their relay logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedisConfig {
    /// Host name or address, without a port.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// ACL user, when the server is not using the `default` user.
    pub username: Option<String>,
    /// Password, when the server requires one.
    pub password: Option<String>,
    /// Redis logical database index.
    pub database: u32,
    /// Prefix for every stream key this backend owns.
    pub key_prefix: String,
    /// Bound on connecting a socket.
    pub connect_timeout: Duration,
    /// Bound on one Redis read or write.
    pub io_timeout: Duration,
}

impl Default for RedisConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 6_379,
            username: None,
            password: None,
            database: 0,
            key_prefix: "loom:relay".into(),
            connect_timeout: Duration::from_secs(2),
            io_timeout: Duration::from_secs(5),
        }
    }
}

impl RedisConfig {
    /// A configuration for `host:port` with every other default in place.
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            ..Self::default()
        }
    }

    /// Parses a `redis://[user][:password]@host[:port][/db]` URL.
    ///
    /// `rediss://` is rejected rather than silently downgraded: TLS needs a
    /// dependency this crate does not carry, so the supported answer is a TLS
    /// terminator in front of Redis (see `docs/redis-backend.md`).
    pub fn from_url(url: &str) -> Result<Self> {
        let authority_and_path = if let Some(rest) = url.strip_prefix("redis://") {
            rest
        } else if url.starts_with("rediss://") {
            return Err(RelayError::config(
                "rediss:// (TLS) is not supported; terminate TLS in front of Redis and connect with redis://",
            ));
        } else {
            return Err(RelayError::config(
                "redis URL must start with redis:// (or set host/port directly)",
            ));
        };
        // Parameters and fragments are not part of this client's vocabulary;
        // drop them rather than misreading them as part of the host.
        let authority_and_path = authority_and_path
            .split(['?', '#'])
            .next()
            .unwrap_or(authority_and_path);

        let (authority, database) = match authority_and_path.split_once('/') {
            Some((authority, path)) => (authority, parse_database(path)?),
            None => (authority_and_path, 0),
        };

        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((userinfo, hostport)) => (Some(userinfo), hostport),
            None => (None, authority),
        };
        let (username, password) = match userinfo {
            None => (None, None),
            Some(info) => match info.split_once(':') {
                Some((user, secret)) => (
                    non_empty(percent_decode(user)?),
                    non_empty(percent_decode(secret)?),
                ),
                // `user@host` is a username without a password; write
                // `redis://:pass@host` for a password-only server.
                None => (non_empty(percent_decode(info)?), None),
            },
        };

        let (host, port) = split_host_port(hostport)?;
        let config = Self {
            host,
            port,
            username,
            password,
            database,
            ..Self::default()
        };
        config.validate()?;
        Ok(config)
    }

    /// Rejects settings that cannot produce a working connection.
    pub fn validate(&self) -> Result<()> {
        if self.host.trim().is_empty() {
            return Err(RelayError::config("redis host must not be empty"));
        }
        if self.port == 0 {
            return Err(RelayError::config("redis port must not be zero"));
        }
        if self.key_prefix.trim().is_empty() {
            return Err(RelayError::config("redis key_prefix must not be empty"));
        }
        Ok(())
    }

    /// The `host:port` string a socket is opened against.
    pub fn addr(&self) -> String {
        let host = if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        format!("{host}:{}", self.port)
    }

    /// The stream key backing `shard`.
    pub fn shard_key(&self, shard: ShardId) -> String {
        format!("{}:shard:{shard}", self.key_prefix)
    }

    /// Applies this configuration's identity (user, password, database) to a
    /// freshly opened connection.
    fn authenticate(&self, connection: &mut Connection) -> Result<()> {
        if let Some(password) = &self.password {
            let reply = match &self.username {
                Some(username) => {
                    connection.command(&[b"AUTH", username.as_bytes(), password.as_bytes()])?
                }
                None => connection.command(&[b"AUTH", password.as_bytes()])?,
            };
            expect_simple(&reply, "AUTH")?;
        }
        if self.database != 0 {
            let database = self.database.to_string();
            let reply = connection.command(&[b"SELECT", database.as_bytes()])?;
            expect_simple(&reply, "SELECT")?;
        }
        Ok(())
    }
}

/// A [`RelayBackend`] that keeps the log in Redis Streams, shared by every
/// node that points at the same Redis and key prefix.
pub struct RedisBackend {
    config: RedisConfig,
    max_len: usize,
    /// One lazily opened connection per shard.
    shards: Vec<Mutex<Option<Connection>>>,
}

impl RedisBackend {
    /// Opens the shared log.
    ///
    /// Connects and `PING`s immediately so a wrong address or password fails
    /// at startup instead of on the first event. The other shards connect on
    /// first use.
    pub fn open(config: RedisConfig, max_len: usize) -> Result<Self> {
        config.validate()?;
        let backend = Self {
            config,
            max_len: max_len.max(1),
            shards: (0..SHARD_COUNT).map(|_| Mutex::new(None)).collect(),
        };
        backend.ping()?;
        Ok(backend)
    }

    /// The effective configuration, including any host/port defaults.
    pub fn config(&self) -> &RedisConfig {
        &self.config
    }

    /// The configured per-shard entry cap.
    pub fn max_len(&self) -> usize {
        self.max_len
    }

    /// Round-trips a `PING`, proving the broker is reachable with the current
    /// credentials.
    pub fn ping(&self) -> Result<()> {
        let reply = self.with_connection(0, |connection| connection.command(&[b"PING"]))?;
        expect_simple(&reply, "PING").map(|_| ())
    }

    /// Deletes every stream this backend owns.
    ///
    /// Destructive, and intended for test teardown and for an operator who
    /// wants to drop the shared window before switching deployments. It does
    /// not touch any other key.
    pub fn purge(&self) -> Result<()> {
        let keys: Vec<String> = (0..SHARD_COUNT)
            .map(|shard| self.config.shard_key(shard))
            .collect();
        let mut args: Vec<&[u8]> = Vec::with_capacity(keys.len() + 1);
        args.push(b"DEL");
        for key in &keys {
            args.push(key.as_bytes());
        }
        let mut connection = self.connect()?;
        connection.command(&args)?;
        Ok(())
    }

    fn connect(&self) -> Result<Connection> {
        let mut connection = Connection::connect(&self.config.addr(), self.config.connect_timeout)?;
        self.config.authenticate(&mut connection)?;
        Ok(connection)
    }

    fn shard(&self, shard: ShardId) -> Result<&Mutex<Option<Connection>>> {
        self.shards.get(usize::from(shard)).ok_or_else(|| {
            RelayError::backend(format!("shard {shard} out of range (0..{SHARD_COUNT})"))
        })
    }

    /// Runs one command on a shard's connection, reconnecting once on failure.
    ///
    /// A failed command may have died at any point in the response, leaving
    /// the socket desynchronised, so the connection is always discarded rather
    /// than reused.
    fn with_connection<R>(
        &self,
        shard: ShardId,
        mut operation: impl FnMut(&mut Connection) -> Result<R>,
    ) -> Result<R> {
        let slot = self.shard(shard)?;
        let mut guard = lock(slot);
        if guard.is_none() {
            *guard = Some(self.connect()?);
        }

        let first = {
            let connection = guard.as_mut().expect("connection just installed");
            operation(connection)
        };
        let first_error = match first {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };

        *guard = None;
        let mut connection = self.connect()?;
        match operation(&mut connection) {
            Ok(value) => {
                *guard = Some(connection);
                Ok(value)
            }
            Err(retry_error) => Err(RelayError::backend(format!(
                "redis shard {shard} command failed: {first_error}; after reconnect: {retry_error}"
            ))),
        }
    }
}

impl RelayBackend for RedisBackend {
    fn shard_count(&self) -> u8 {
        SHARD_COUNT
    }

    fn append(&self, shard: ShardId, record: LogRecord) -> Result<()> {
        let key = self.config.shard_key(shard);
        let owned = xadd_args(&key, self.max_len, &record);
        let args = as_args(&owned);
        let reply = self.with_connection(shard, |connection| connection.command(&args))?;
        // Success is the assigned stream id; a `-ERR` reply becomes an error.
        reply.into_bytes("XADD")?;
        Ok(())
    }

    /// Reads entries newer than `after`.
    ///
    /// `XRANGE` is bounded server-side by a start id anchored to the cursor's
    /// millisecond, so resuming does not scan the whole stream. Stream ids are
    /// time-ordered, and an [`EventId`] carries the same millisecond, so the
    /// `{ms}-0` anchor cannot skip a record the cursor has not passed: any
    /// entry after the cursor has a stream id in the same or a later
    /// millisecond. Records in that millisecond that are at or before the
    /// cursor are then filtered by full event id.
    fn read_after(
        &self,
        shard: ShardId,
        after: Option<EventId>,
        limit: usize,
    ) -> Result<Vec<LogRecord>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let key = self.config.shard_key(shard);
        let start = match after {
            None => b"-".to_vec(),
            Some(cursor) => format!("{}-0", cursor.timestamp_ms()).into_bytes(),
        };
        let reply = self.with_connection(shard, |connection| {
            connection.command(&[b"XRANGE", key.as_bytes(), start.as_slice(), b"+"])
        })?;
        let entries = reply.into_array("XRANGE")?;

        let mut records = Vec::new();
        for entry in &entries {
            let decoded = parse_entry(entry)?;
            if after.is_some_and(|cursor| decoded.record.event_id <= cursor) {
                continue;
            }
            records.push(decoded.record);
            if records.len() >= limit {
                break;
            }
        }
        Ok(records)
    }

    /// Removes every entry older than `before_ms`, returning how many were
    /// actually deleted.
    ///
    /// The scan is over the shard's own stream, which `MAXLEN` keeps bounded,
    /// and deletion is batched. Doing it this way rather than by stream id
    /// keeps `created_at_ms` the single cut-off — a backfilled record is
    /// trimmed by its real timestamp, exactly as the in-process backend does.
    /// A record another node appended but this node has not read yet is still
    /// deleted, because Redis, not the local view, is the authority.
    fn trim(&self, shard: ShardId, before_ms: u64) -> Result<u64> {
        let key = self.config.shard_key(shard);
        let reply = self.with_connection(shard, |connection| {
            connection.command(&[b"XRANGE", key.as_bytes(), b"-", b"+"])
        })?;
        let entries = reply.into_array("XRANGE")?;

        let mut doomed: Vec<Vec<u8>> = Vec::new();
        for entry in &entries {
            let decoded = parse_entry(entry)?;
            if decoded.record.created_at_ms < before_ms {
                doomed.push(decoded.stream_id);
            }
        }
        if doomed.is_empty() {
            return Ok(0);
        }

        let mut removed = 0u64;
        for batch in doomed.chunks(TRIM_BATCH) {
            let mut owned: Vec<Vec<u8>> = Vec::with_capacity(batch.len() + 2);
            owned.push(b"XDEL".to_vec());
            owned.push(key.as_bytes().to_vec());
            owned.extend(batch.iter().cloned());
            let args = as_args(&owned);
            let reply = self.with_connection(shard, |connection| connection.command(&args))?;
            removed += u64::try_from(reply.into_int("XDEL")?)
                .map_err(|_| RelayError::backend("XDEL returned a negative count"))?;
        }
        Ok(removed)
    }

    fn len(&self, shard: ShardId) -> Result<usize> {
        let key = self.config.shard_key(shard);
        let reply = self.with_connection(shard, |connection| {
            connection.command(&[b"XLEN", key.as_bytes()])
        })?;
        usize::try_from(reply.into_int("XLEN")?)
            .map_err(|_| RelayError::backend("XLEN returned a negative count"))
    }
}

impl std::fmt::Debug for RedisBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisBackend")
            .field("addr", &self.config.addr())
            .field("key_prefix", &self.config.key_prefix)
            .field("max_len", &self.max_len)
            .field("shards", &SHARD_COUNT)
            .finish_non_exhaustive()
    }
}

/// One decoded stream entry.
struct DecodedEntry {
    /// The Redis-assigned stream id.
    stream_id: Vec<u8>,
    /// The relay record it carries.
    record: LogRecord,
}

/// Builds the full `XADD` argument vector for a record.
fn xadd_args(key: &str, max_len: usize, record: &LogRecord) -> Vec<Vec<u8>> {
    vec![
        b"XADD".to_vec(),
        key.as_bytes().to_vec(),
        b"MAXLEN".to_vec(),
        b"=".to_vec(),
        max_len.to_string().into_bytes(),
        b"*".to_vec(),
        FIELD_EVENT_ID.to_vec(),
        record.event_id.to_base32().into_bytes(),
        FIELD_CREATED_AT.to_vec(),
        record.created_at_ms.to_string().into_bytes(),
        FIELD_ORIGIN.to_vec(),
        record.origin.as_bytes().to_vec(),
        FIELD_SCOPE_KIND.to_vec(),
        record.scope.kind().as_bytes().to_vec(),
        FIELD_SCOPE_ID.to_vec(),
        record.scope.id().as_bytes().to_vec(),
        FIELD_PAYLOAD.to_vec(),
        record.payload.to_vec(),
    ]
}

/// Borrows an owned argument vector as the slice-of-slices a command wants.
fn as_args(owned: &[Vec<u8>]) -> Vec<&[u8]> {
    owned.iter().map(Vec::as_slice).collect()
}

/// Decodes one `XRANGE` entry into a record.
///
/// Every field this backend writes is required; an unknown field is ignored so
/// that a future writer can add one without breaking this reader.
fn parse_entry(value: &Value) -> Result<DecodedEntry> {
    let items = value
        .as_array()
        .ok_or_else(|| RelayError::backend("XRANGE entry is not an [id, fields] pair"))?;
    if items.len() != 2 {
        return Err(RelayError::backend(format!(
            "XRANGE entry has {} elements, expected 2",
            items.len()
        )));
    }
    let stream_id = items[0]
        .as_bytes()
        .ok_or_else(|| RelayError::backend("XRANGE entry id is not a bulk string"))?
        .to_vec();
    let fields = items[1]
        .as_array()
        .ok_or_else(|| RelayError::backend("XRANGE entry fields are not an array"))?;
    if fields.len() % 2 != 0 {
        return Err(RelayError::backend(
            "XRANGE entry has an odd number of field elements",
        ));
    }

    let mut event_id = None;
    let mut created_at_ms = None;
    let mut origin = None;
    let mut kind = None;
    let mut scope_id = None;
    let mut payload = None;

    for pair in fields.chunks(2) {
        let name = pair[0]
            .as_bytes()
            .ok_or_else(|| RelayError::backend("XRANGE field name is not a bulk string"))?;
        let raw = pair[1]
            .as_bytes()
            .ok_or_else(|| RelayError::backend("XRANGE field value is not a bulk string"))?;
        match name {
            FIELD_EVENT_ID => event_id = Some(parse_event_id(raw)?),
            FIELD_CREATED_AT => created_at_ms = Some(parse_u64(raw, "created_at_ms")?),
            FIELD_ORIGIN => origin = Some(text(raw, "origin")?),
            FIELD_SCOPE_KIND => kind = Some(text(raw, "scope kind")?),
            FIELD_SCOPE_ID => scope_id = Some(text(raw, "scope id")?),
            FIELD_PAYLOAD => payload = Some(Bytes::copy_from_slice(raw)),
            _ => {}
        }
    }

    let kind = required(kind, "scope kind")?;
    let scope_id = required(scope_id, "scope id")?;
    let scope = Scope::from_kind_id(&kind, scope_id)
        .ok_or_else(|| RelayError::backend(format!("unknown scope kind {kind:?}")))?;

    Ok(DecodedEntry {
        stream_id,
        record: LogRecord {
            event_id: required(event_id, "event id")?,
            scope,
            payload: required(payload, "payload")?,
            created_at_ms: required(created_at_ms, "created_at_ms")?,
            origin: required(origin, "origin")?,
        },
    })
}

fn required<T>(value: Option<T>, field: &str) -> Result<T> {
    value.ok_or_else(|| RelayError::backend(format!("redis record is missing field {field}")))
}

fn parse_event_id(raw: &[u8]) -> Result<EventId> {
    let text =
        std::str::from_utf8(raw).map_err(|_| RelayError::backend("event id is not UTF-8"))?;
    text.parse::<EventId>()
        .map_err(|error| RelayError::InvalidEventId(error.to_string()))
}

fn parse_u64(raw: &[u8], field: &str) -> Result<u64> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| RelayError::backend(format!("{field} is not UTF-8")))?;
    text.parse::<u64>()
        .map_err(|_| RelayError::backend(format!("{field}: {text:?} is not a u64")))
}

fn text(raw: &[u8], field: &str) -> Result<String> {
    String::from_utf8(raw.to_vec())
        .map_err(|_| RelayError::backend(format!("{field} is not UTF-8")))
}

fn expect_simple(value: &Value, context: &str) -> Result<()> {
    match value {
        Value::Simple(_) => Ok(()),
        Value::Error(message) => Err(RelayError::backend(format!("{context} failed: {message}"))),
        other => Err(RelayError::backend(format!(
            "{context}: expected a simple string, got {other:?}"
        ))),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn non_empty(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn parse_database(path: &str) -> Result<u32> {
    if path.is_empty() || path == "/" {
        return Ok(0);
    }
    path.trim_end_matches('/')
        .parse::<u32>()
        .map_err(|_| RelayError::config(format!("redis URL database {path:?} is not a number")))
}

fn split_host_port(hostport: &str) -> Result<(String, u16)> {
    if hostport.is_empty() {
        return Err(RelayError::config("redis URL has no host"));
    }
    if let Some(inner) = hostport.strip_prefix('[') {
        let (host, rest) = inner
            .split_once(']')
            .ok_or_else(|| RelayError::config("redis URL has an unterminated IPv6 literal"))?;
        let port = match rest {
            "" => 6_379,
            other => match other.strip_prefix(':') {
                Some(port) => parse_port(port)?,
                None => {
                    return Err(RelayError::config(
                        "unexpected text after the IPv6 literal in the redis URL",
                    ))
                }
            },
        };
        return Ok((host.to_string(), port));
    }
    if hostport.matches(':').count() > 1 {
        return Err(RelayError::config(
            "IPv6 hosts must be bracketed, as in redis://[::1]:6379",
        ));
    }
    match hostport.rsplit_once(':') {
        Some((host, port)) => {
            if host.is_empty() {
                return Err(RelayError::config("redis URL has no host"));
            }
            Ok((host.to_string(), parse_port(port)?))
        }
        None => Ok((hostport.to_string(), 6_379)),
    }
}

fn parse_port(port: &str) -> Result<u16> {
    port.parse::<u16>()
        .map_err(|_| RelayError::config(format!("redis URL port {port:?} is not a number")))
}

/// Decodes `%XX` escapes, as used in URL userinfo.
fn percent_decode(input: &str) -> Result<String> {
    if !input.contains('%') {
        return Ok(input.to_string());
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(RelayError::config("truncated percent escape in redis URL"));
            }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                .map_err(|_| RelayError::config("invalid percent escape in redis URL"))?;
            let byte = u8::from_str_radix(hex, 16)
                .map_err(|_| RelayError::config("invalid percent escape in redis URL"))?;
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out)
        .map_err(|_| RelayError::config("redis URL is not valid UTF-8 after decoding"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> LogRecord {
        LogRecord {
            event_id: EventId::from_raw(0x0123_4567_89AB_CDEF_0123_4567_89AB_CDEF),
            scope: Scope::Thread("thr_1".into()),
            payload: Bytes::from_static(b"{\"hello\":\"loom\"}"),
            created_at_ms: 1_700_000_000_000,
            origin: "node-a".into(),
        }
    }

    #[test]
    fn url_defaults_fill_in_port_database_and_prefix() {
        let config = RedisConfig::from_url("redis://127.0.0.1").unwrap();
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 6_379);
        assert_eq!(config.database, 0);
        assert_eq!(config.key_prefix, "loom:relay");
        assert!(config.password.is_none());
    }

    #[test]
    fn url_parses_every_declared_part() {
        let config = RedisConfig::from_url("redis://loom:p%40ss@redis.internal:6380/3").unwrap();
        assert_eq!(config.host, "redis.internal");
        assert_eq!(config.port, 6380);
        assert_eq!(config.username.as_deref(), Some("loom"));
        assert_eq!(config.password.as_deref(), Some("p@ss"));
        assert_eq!(config.database, 3);
    }

    #[test]
    fn url_accepts_password_only_and_ipv6() {
        let config = RedisConfig::from_url("redis://:secret@127.0.0.1:6379/0").unwrap();
        assert!(config.username.is_none());
        assert_eq!(config.password.as_deref(), Some("secret"));

        let config = RedisConfig::from_url("redis://[::1]:6380/1").unwrap();
        assert_eq!(config.host, "::1");
        assert_eq!(config.port, 6380);
        assert_eq!(config.addr(), "[::1]:6380");
    }

    #[test]
    fn url_ignores_query_parameters_and_trailing_slash() {
        let config = RedisConfig::from_url("redis://user@host/?timeout=1&tls=false").unwrap();
        assert_eq!(config.username.as_deref(), Some("user"));
        assert!(config.password.is_none());
        assert_eq!(config.database, 0);
    }

    #[test]
    fn url_rejects_unsupported_or_malformed_forms() {
        assert!(RedisConfig::from_url("rediss://host").is_err());
        assert!(RedisConfig::from_url("http://host").is_err());
        assert!(RedisConfig::from_url("redis://").is_err());
        assert!(RedisConfig::from_url("redis://host:not-a-port").is_err());
        assert!(RedisConfig::from_url("redis://host/not-a-db").is_err());
        assert!(RedisConfig::from_url("redis://::1:6379").is_err());
    }

    #[test]
    fn every_shard_has_a_distinct_key() {
        let config = RedisConfig::default();
        let keys: std::collections::HashSet<String> =
            (0..SHARD_COUNT).map(|s| config.shard_key(s)).collect();
        assert_eq!(keys.len(), usize::from(SHARD_COUNT));
        assert!(keys.contains("loom:relay:shard:0"));
    }

    #[test]
    fn xadd_args_round_trip_through_the_decoder() {
        let original = record();
        let key = "loom:test:shard:0";
        let args = xadd_args(key, 100, &original);

        // The first six arguments are the command and its trim options; the
        // rest are the field/value pairs Redis stores.
        assert_eq!(args[0], b"XADD");
        assert_eq!(args[1], key.as_bytes());
        assert_eq!(args[2], b"MAXLEN");
        assert_eq!(args[3], b"=");
        assert_eq!(args[4], b"100");
        assert_eq!(args[5], b"*");

        let pairs = args[6..]
            .iter()
            .map(|part| Value::Bulk(Some(part.clone())))
            .collect();
        let entry = Value::Array(Some(vec![
            Value::Bulk(Some(b"1700000000000-0".to_vec())),
            Value::Array(Some(pairs)),
        ]));

        let decoded = parse_entry(&entry).unwrap();
        assert_eq!(decoded.stream_id, b"1700000000000-0");
        assert_eq!(decoded.record, original);
    }

    #[test]
    fn decoder_ignores_unknown_fields() {
        let original = record();
        let mut args = xadd_args("k", 10, &original);
        args.push(b"future".to_vec());
        args.push(b"field".to_vec());
        let pairs = args[6..]
            .iter()
            .map(|part| Value::Bulk(Some(part.clone())))
            .collect();
        let entry = Value::Array(Some(vec![
            Value::Bulk(Some(b"1-1".to_vec())),
            Value::Array(Some(pairs)),
        ]));
        assert_eq!(parse_entry(&entry).unwrap().record, original);
    }

    #[test]
    fn decoder_rejects_incomplete_or_malformed_entries() {
        let missing = Value::Array(Some(vec![
            Value::Bulk(Some(b"1-1".to_vec())),
            Value::Array(Some(vec![Value::Bulk(Some(b"e".to_vec()))])),
        ]));
        assert!(parse_entry(&missing).is_err());

        let scalar = Value::Bulk(Some(b"nope".to_vec()));
        assert!(parse_entry(&scalar).is_err());

        let bad_scope = Value::Array(Some(vec![
            Value::Bulk(Some(b"1-1".to_vec())),
            Value::Array(Some(vec![
                Value::Bulk(Some(b"e".to_vec())),
                Value::Bulk(Some(EventId::new().to_base32().into_bytes())),
                Value::Bulk(Some(b"t".to_vec())),
                Value::Bulk(Some(b"1".to_vec())),
                Value::Bulk(Some(b"o".to_vec())),
                Value::Bulk(Some(b"n".to_vec())),
                Value::Bulk(Some(b"k".to_vec())),
                Value::Bulk(Some(b"planet".to_vec())),
                Value::Bulk(Some(b"i".to_vec())),
                Value::Bulk(Some(b"x".to_vec())),
                Value::Bulk(Some(b"p".to_vec())),
                Value::Bulk(Some(b"{}".to_vec())),
            ])),
        ]));
        assert!(parse_entry(&bad_scope).is_err());
    }

    #[test]
    fn config_validation_rejects_empty_values() {
        let config = RedisConfig {
            host: String::new(),
            ..RedisConfig::default()
        };
        assert!(config.validate().is_err());

        let config = RedisConfig {
            port: 0,
            ..RedisConfig::default()
        };
        assert!(config.validate().is_err());

        let config = RedisConfig {
            key_prefix: "  ".into(),
            ..RedisConfig::default()
        };
        assert!(config.validate().is_err());
    }
}

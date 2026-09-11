//! A minimal RESP2 client.
//!
//! The shared backend needs exactly four Redis commands (`XADD`, `XRANGE`,
//! `XDEL`, `XLEN`) plus `PING`, `AUTH` and `SELECT` on connect. That is a tiny
//! slice of the protocol, and RESP is line-oriented and small enough to read in
//! one sitting, so it is implemented here instead of pulling in `redis-rs` and
//! its dependency tree. The project's default deployment stays zero-dependency;
//! a Redis backend is configuration, not a new compile-time universe.
//!
//! Only RESP2 is spoken. Every value we send is a byte string, which keeps the
//! client binary-safe: a relay payload is bytes, and it must survive the round
//! trip unchanged.
//!
//! The cost of this choice is that the client is deliberately partial — no
//! pipelining, no RESP3, no cluster redirects, no TLS. All of those are listed
//! as deployment requirements in `docs/redis-backend.md`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::error::{RelayError, Result};

/// One decoded RESP reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Value {
    /// `+OK`
    Simple(String),
    /// `-ERR ...`
    Error(String),
    /// `:42`
    Int(i64),
    /// `$3\r\nfoo\r\n`, or a null bulk string.
    Bulk(Option<Vec<u8>>),
    /// An array, or a null array.
    Array(Option<Vec<Value>>),
}

impl Value {
    /// The bytes of a bulk string, or an error naming the actual reply.
    pub(super) fn into_bytes(self, context: &str) -> Result<Vec<u8>> {
        match self {
            Value::Bulk(Some(bytes)) => Ok(bytes),
            other => Err(RelayError::backend(format!(
                "{context}: expected a bulk string, got {}",
                other.describe()
            ))),
        }
    }

    /// The payload of an array reply.
    pub(super) fn into_array(self, context: &str) -> Result<Vec<Value>> {
        match self {
            Value::Array(Some(items)) => Ok(items),
            other => Err(RelayError::backend(format!(
                "{context}: expected an array, got {}",
                other.describe()
            ))),
        }
    }

    /// The payload of an integer reply.
    pub(super) fn into_int(self, context: &str) -> Result<i64> {
        match self {
            Value::Int(number) => Ok(number),
            other => Err(RelayError::backend(format!(
                "{context}: expected an integer, got {}",
                other.describe()
            ))),
        }
    }

    /// The bytes of a bulk string, if this is one.
    pub(super) fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bulk(Some(bytes)) => Some(bytes),
            _ => None,
        }
    }

    /// The elements of an array, if this is one.
    pub(super) fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(Some(items)) => Some(items),
            _ => None,
        }
    }

    /// A human-readable label for diagnostics.
    fn describe(&self) -> String {
        match self {
            Value::Simple(text) => format!("simple string {text:?}"),
            Value::Error(text) => format!("an error reply: {text}"),
            Value::Int(number) => format!("integer {number}"),
            Value::Bulk(None) => "a null bulk string".into(),
            Value::Bulk(Some(bytes)) => format!("a {}-byte bulk string", bytes.len()),
            Value::Array(None) => "a null array".into(),
            Value::Array(Some(items)) => format!("an array of {}", items.len()),
        }
    }
}

/// A single TCP connection to a Redis server.
///
/// Redis is request/response per connection, so one connection is used by one
/// caller at a time; the backend serialises access with a mutex per shard.
pub(super) struct Connection {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("peer", &self.writer.peer_addr().ok())
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// Connects to `addr` (a `host:port` pair, IPv6 bracketed), applying
    /// `timeout` to connect, reads and writes.
    pub(super) fn connect(addr: &str, timeout: Duration) -> Result<Self> {
        let mut resolved = addr
            .to_socket_addrs()
            .map_err(|error| RelayError::backend(format!("resolving {addr}: {error}")))?;
        let socket = resolved
            .next()
            .ok_or_else(|| RelayError::backend(format!("{addr} resolved to no addresses")))?;

        let stream = TcpStream::connect_timeout(&socket, timeout)
            .map_err(|error| RelayError::backend(format!("connecting to {addr}: {error}")))?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|error| RelayError::backend(format!("setting read timeout: {error}")))?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|error| RelayError::backend(format!("setting write timeout: {error}")))?;
        // A relay frame is latency-sensitive and small; Nagle only adds delay.
        stream
            .set_nodelay(true)
            .map_err(|error| RelayError::backend(format!("setting TCP_NODELAY: {error}")))?;

        let writer = stream
            .try_clone()
            .map_err(|error| RelayError::backend(format!("cloning socket: {error}")))?;
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
        })
    }

    /// Sends one command and reads exactly one reply.
    ///
    /// A `-ERR` reply is **not** an error here: the caller decides whether a
    /// particular command's error is meaningful. IO and framing problems are
    /// errors, because they leave the connection unusable.
    pub(super) fn command(&mut self, args: &[&[u8]]) -> Result<Value> {
        self.write_command(args)?;
        self.read_reply()
    }

    fn write_command(&mut self, args: &[&[u8]]) -> Result<()> {
        let mut request = Vec::new();
        request.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
        for arg in args {
            request.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
            request.extend_from_slice(arg);
            request.extend_from_slice(b"\r\n");
        }
        self.writer
            .write_all(&request)
            .map_err(|error| RelayError::backend(format!("writing command: {error}")))?;
        self.writer
            .flush()
            .map_err(|error| RelayError::backend(format!("flushing command: {error}")))
    }

    fn read_reply(&mut self) -> Result<Value> {
        let line = self.read_line()?;
        let (prefix, rest) = line
            .split_first()
            .ok_or_else(|| RelayError::backend("empty RESP reply"))?;
        match prefix {
            b'+' => Ok(Value::Simple(lossy(rest))),
            b'-' => Ok(Value::Error(lossy(rest))),
            b':' => Ok(Value::Int(parse_int(rest, "integer reply")?)),
            b'$' => {
                let len = parse_int(rest, "bulk length")?;
                if len < 0 {
                    return Ok(Value::Bulk(None));
                }
                let mut body = vec![0u8; len as usize];
                self.reader
                    .read_exact(&mut body)
                    .map_err(|error| RelayError::backend(format!("reading bulk body: {error}")))?;
                self.expect_crlf()?;
                Ok(Value::Bulk(Some(body)))
            }
            b'*' => {
                let count = parse_int(rest, "array length")?;
                if count < 0 {
                    return Ok(Value::Array(None));
                }
                let mut items = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    items.push(self.read_reply()?);
                }
                Ok(Value::Array(Some(items)))
            }
            other => Err(RelayError::backend(format!(
                "unknown RESP reply byte {:?}",
                char::from(*other)
            ))),
        }
    }

    fn read_line(&mut self) -> Result<Vec<u8>> {
        let mut line = Vec::new();
        let read = self
            .reader
            .read_until(b'\n', &mut line)
            .map_err(|error| RelayError::backend(format!("reading reply line: {error}")))?;
        if read == 0 {
            return Err(RelayError::backend("connection closed by redis"));
        }
        if !line.ends_with(b"\r\n") {
            return Err(RelayError::backend("RESP line without CRLF terminator"));
        }
        line.truncate(line.len() - 2);
        Ok(line)
    }

    fn expect_crlf(&mut self) -> Result<()> {
        let mut terminator = [0u8; 2];
        self.reader
            .read_exact(&mut terminator)
            .map_err(|error| RelayError::backend(format!("reading bulk terminator: {error}")))?;
        if terminator != *b"\r\n" {
            return Err(RelayError::backend("bulk string without CRLF terminator"));
        }
        Ok(())
    }
}

fn parse_int(bytes: &[u8], context: &str) -> Result<i64> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| RelayError::backend(format!("{context}: not ASCII")))?;
    text.parse::<i64>()
        .map_err(|_| RelayError::backend(format!("{context}: {text:?} is not an integer")))
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes a reply as bytes, then parses it via an in-memory reader.
    fn parse(reply: &[u8]) -> Result<Value> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let payload = reply.to_vec();
        let server = std::thread::spawn(move || {
            use std::io::Write as _;
            let (mut socket, _) = listener.accept().unwrap();
            socket.write_all(&payload).unwrap();
        });
        let mut connection =
            Connection::connect(&addr.to_string(), Duration::from_secs(2)).unwrap();
        let value = connection.read_reply();
        server.join().unwrap();
        value
    }

    #[test]
    fn parses_every_resp2_type() {
        assert_eq!(parse(b"+OK\r\n").unwrap(), Value::Simple("OK".into()));
        assert_eq!(parse(b":7\r\n").unwrap(), Value::Int(7));
        assert_eq!(
            parse(b"$3\r\nfoo\r\n").unwrap(),
            Value::Bulk(Some(b"foo".to_vec()))
        );
        assert_eq!(parse(b"$-1\r\n").unwrap(), Value::Bulk(None));
        assert_eq!(parse(b"*-1\r\n").unwrap(), Value::Array(None));
        assert_eq!(
            parse(b"*2\r\n$1\r\na\r\n:2\r\n").unwrap(),
            Value::Array(Some(vec![Value::Bulk(Some(b"a".to_vec())), Value::Int(2)]))
        );
    }

    #[test]
    fn error_replies_decode_rather_than_fail() {
        assert_eq!(
            parse(b"-WRONGTYPE bad\r\n").unwrap(),
            Value::Error("WRONGTYPE bad".into())
        );
    }

    #[test]
    fn payloads_are_binary_safe() {
        let value = parse(b"$4\r\n\x00\xff\r\n\r\n").unwrap();
        assert_eq!(value, Value::Bulk(Some(vec![0, 0xff, b'\r', b'\n'])));
    }

    #[test]
    fn truncated_replies_are_errors() {
        assert!(parse(b"$3\r\nfo").is_err());
        assert!(parse(b"*1\r\n").is_err());
        assert!(parse(b"+no terminator").is_err());
    }

    #[test]
    fn writes_commands_in_resp_format() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            use std::io::Read as _;
            let (mut socket, _) = listener.accept().unwrap();
            let mut received = Vec::new();
            socket.read_to_end(&mut received).unwrap();
            received
        });
        let mut connection =
            Connection::connect(&addr.to_string(), Duration::from_secs(2)).unwrap();
        // Write directly; reading a reply is covered above.
        connection
            .write_command(&[b"XADD", b"k", b"*", b"f", b"v"])
            .unwrap();
        drop(connection);
        let sent = server.join().unwrap();
        assert_eq!(
            sent,
            b"*5\r\n$4\r\nXADD\r\n$1\r\nk\r\n$1\r\n*\r\n$1\r\nf\r\n$1\r\nv\r\n"
        );
    }

    #[test]
    fn value_accessors_reject_the_wrong_shape() {
        assert!(Value::Bulk(None).into_bytes("x").is_err());
        assert!(Value::Simple("OK".into()).into_array("x").is_err());
        assert!(Value::Bulk(Some(b"1".to_vec())).into_int("x").is_err());
    }
}

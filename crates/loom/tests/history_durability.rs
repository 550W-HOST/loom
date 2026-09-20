//! What a killed server leaves behind.
//!
//! A conversation is published before it is written, so the server must survive
//! a stop it did not get to prepare for: what it had already committed has to be
//! there when the next process opens the same directory, and the promise that
//! the last *uncommitted* commits may be lost is only honest if the committed
//! ones are not.
//!
//! Only two real processes can show this, so the server is a child here, killed
//! with `SIGKILL` — no shutdown path, no flush, no chance to tidy up — and the
//! next process reads what the previous one wrote.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::{Child, Command};

/// The binary under test, built by cargo for this integration test.
const BINARY: &str = env!("CARGO_BIN_EXE_loom");

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("read the ephemeral port")
        .port()
}

/// A minimal HTTP/1.1 exchange. The test needs a status and a body, not a
/// client library.
fn request(port: u16, method: &str, path: &str, body: Option<&str>) -> Option<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let body = body.unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .ok()?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).ok()?;
    let status = raw.split_whitespace().nth(1)?.parse().ok()?;
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned();
    Some((status, body))
}

async fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// A server child, reaped even when a test fails.
struct Server {
    child: Child,
}

impl Server {
    fn spawn(port: u16, data_dir: &Path) -> Self {
        let child = Command::new(BINARY)
            .arg("server")
            .arg("--bind")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--data-dir")
            .arg(data_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("the server starts");
        Self { child }
    }

    /// Kills the server the way a machine that is going away does.
    async fn kill_hard(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

/// Starts a server and waits until it answers.
async fn start(port: u16, data_dir: &Path) -> Server {
    let server = Server::spawn(port, data_dir);
    wait_until("the server to answer", || {
        request(port, "GET", "/api/v1/system/version", None).is_some()
    })
    .await;
    server
}

#[tokio::test]
async fn a_killed_server_leaves_the_conversation_it_committed() {
    let data_dir = tempfile::TempDir::new().unwrap();
    let port = free_port();
    let mut server = start(port, data_dir.path()).await;

    // A thread in the personal project, with no environment: this test is about
    // the conversation, and binding one would need a worker.
    let (status, body) = request(
        port,
        "POST",
        "/api/v1/threads",
        Some(
            r#"{"projectId":"proj_personal","origin":"app","input":[],
                "environment":{"type":"project-default"},"title":"killed"}"#,
        ),
    )
    .expect("the server answers");
    assert!(
        (200..300).contains(&status),
        "the thread is created: {status} {body}"
    );
    let created: serde_json::Value = serde_json::from_str(&body).expect("a JSON thread");
    let thread_id = created["id"]
        .as_str()
        .expect("the response names the thread")
        .to_owned();

    let (status, body) = request(
        port,
        "POST",
        &format!("/api/v1/threads/{thread_id}/messages"),
        Some(r#"{"content":"survives a kill"}"#),
    )
    .expect("the server answers");
    assert!(
        (200..300).contains(&status),
        "the message is published: {status} {body}"
    );

    // The publish path returns before the disk does, so there is nothing to poll
    // that only the disk can answer. The row is tiny and the writer is idle; the
    // assertion after the restart is what proves it was committed, and a wait
    // that were too short would fail loudly rather than pass quietly.
    tokio::time::sleep(Duration::from_secs(1)).await;

    server.kill_hard().await;

    // The next process reads what the last one wrote. Nothing is enrolled as a
    // host here, so nothing can load the conversation from an agent: if the
    // message is on the timeline, it came out of the store.
    let next_port = free_port();
    let _next = start(next_port, data_dir.path()).await;
    let (status, timeline) = request(
        next_port,
        "GET",
        &format!("/api/v1/threads/{thread_id}/timeline"),
        None,
    )
    .expect("the restarted server answers");
    assert_eq!(status, 200, "the timeline is served: {timeline}");
    assert!(
        timeline.contains("survives a kill"),
        "the committed message outlived the kill: {timeline}"
    );

    let (_, prompts) = request(
        next_port,
        "GET",
        &format!("/api/v1/threads/{thread_id}/prompt-history"),
        None,
    )
    .expect("the restarted server answers");
    assert!(
        prompts.contains("survives a kill"),
        "and it is still what the user asked: {prompts}"
    );
}

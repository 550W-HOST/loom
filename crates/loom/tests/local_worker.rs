//! `loom server --local-worker`, end to end: the single-box shape.
//!
//! The unit tests in `crates/server/src/local_worker.rs` cover the URL that is
//! derived and the executable that is chosen. This file covers what only two
//! real processes can: that the server actually starts one worker child, that
//! the child enrolls as a host, that the supervisor brings it back after it
//! dies, and that stopping the server takes it away again.
//!
//! Linux only: the child is found through `/proc`, because the worker is spawned
//! by the server process the test holds rather than by the test itself.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpListener;
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

/// A minimal HTTP/1.1 GET. The test talks to a real listener and needs a status
/// and a body, not a client library.
fn http_get(port: u16, path: &str) -> Option<(u16, String)> {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).ok()?;
    let status = raw.split_whitespace().nth(1)?.parse().ok()?;
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned();
    Some((status, body))
}

/// The direct children of `parent`, read from `/proc`.
fn children_of(parent: i32) -> Vec<i32> {
    let mut children = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return children;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // After the parenthesised command name, field 3 is the state and field 4
        // the parent pid — index 1 of the remainder. `/proc/<pid>/stat` is the
        // only place a test can learn a pid it did not spawn itself.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let ppid = rest
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<i32>().ok());
        if ppid == Some(parent) {
            children.push(pid);
        }
    }
    children
}

fn alive(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok()
}

/// Best-effort `kill`; the tests assert via the waits, not the signal's exit.
fn send_signal(pid: i32, signal: &str) {
    let _ = std::process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status();
}

async fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn wait_gone(what: &str, pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if !alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{what} ({pid}) is still running");
}

/// A spawned `loom server --local-worker`, torn down even when a test fails.
///
/// A SIGKILL to the server cannot run its own teardown, so the children are the
/// test's to reap: a leaked worker would outlive the suite and keep reconnecting.
struct Server {
    child: Option<Child>,
    pid: i32,
}

impl Server {
    fn spawn(port: u16, server_data: &Path, extra: &[&str]) -> Self {
        let mut command = Command::new(BINARY);
        command
            .arg("server")
            .arg("--local-worker")
            .args(extra)
            .env("LOOM_BIND", format!("127.0.0.1:{port}"))
            .env("LOOM_DATA_DIR", server_data)
            .env_remove("LOOM_LOCAL_HOST_ID")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let child = command.spawn().expect("spawn loom server --local-worker");
        let pid = child.id().expect("server pid") as i32;
        Self {
            child: Some(child),
            pid,
        }
    }

    fn stop(&self) {
        for worker in children_of(self.pid) {
            send_signal(worker, "KILL");
        }
        send_signal(self.pid, "TERM");
    }

    async fn wait(&mut self) -> std::process::ExitStatus {
        self.child
            .as_mut()
            .expect("server child")
            .wait()
            .await
            .expect("wait for the server")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        for worker in children_of(self.pid) {
            send_signal(worker, "KILL");
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

/// The host id the child persisted under the server's data root.
fn read_host_id(server_data: &Path) -> Option<String> {
    let path = server_data.join("local-worker").join("host-id");
    std::fs::read_to_string(path)
        .ok()
        .map(|raw| raw.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[tokio::test]
async fn a_local_worker_is_started_supervised_and_stopped_with_the_server() {
    let data = tempfile::tempdir().expect("temp dir");
    let port = free_port();
    let server_data = data.path().join("server");
    let mut server = Server::spawn(port, &server_data, &["--local-worker-name", "local-test"]);

    // 1. The server itself is up. It is server-only until the child connects,
    //    so `/health` must not depend on the worker.
    wait_for("the server to answer /health", || {
        http_get(port, "/health").is_some_and(|(status, _)| status == 200)
    })
    .await;

    // 2. The one child enrolled as a host.
    wait_for("the local worker to enroll", || {
        http_get(port, "/api/v1/hosts")
            .is_some_and(|(status, body)| status == 200 && body.contains("\"connected\""))
    })
    .await;

    // 3. The server owns exactly that one child.
    wait_for("exactly one worker child", || {
        children_of(server.pid).len() == 1
    })
    .await;
    let first_worker = children_of(server.pid)[0];

    // 4. Killing the child is recovered by the supervisor, which is also the
    //    path a worker self-update leaves behind (`exit 0`).
    send_signal(first_worker, "KILL");
    wait_for("the supervisor to start a replacement", || {
        children_of(server.pid)
            .iter()
            .any(|pid| *pid != first_worker)
    })
    .await;

    // 5. SIGTERM stops the server *and* the worker child it started.
    let workers = children_of(server.pid);
    assert!(!workers.is_empty(), "the supervisor had no worker to stop");
    send_signal(server.pid, "TERM");
    let status = tokio::time::timeout(Duration::from_secs(30), server.wait())
        .await
        .expect("the server exited on SIGTERM");
    assert!(status.success(), "the server exited with {status:?}");
    for worker in workers {
        wait_gone("the server's worker child", worker).await;
    }
}

/// A restart reuses the persisted host id, so the host list shows one machine
/// rather than two. That is the second half of the supervision contract, and
/// the reason the child is given a state path at all.
#[tokio::test]
async fn a_restarted_local_worker_keeps_one_host_identity() {
    let data = tempfile::tempdir().expect("temp dir");
    let port = free_port();
    let server_data = data.path().join("server");
    let mut server = Server::spawn(port, &server_data, &[]);

    wait_for("the local worker to enroll", || {
        http_get(port, "/api/v1/hosts")
            .is_some_and(|(status, body)| status == 200 && body.contains("\"connected\""))
    })
    .await;
    wait_for("exactly one worker child", || {
        children_of(server.pid).len() == 1
    })
    .await;
    let first_worker = children_of(server.pid)[0];
    let host_id = read_host_id(&server_data).expect("the worker persisted its host id");

    send_signal(first_worker, "KILL");
    wait_for("the supervisor to start a replacement", || {
        children_of(server.pid)
            .iter()
            .any(|pid| *pid != first_worker)
    })
    .await;
    wait_for("one connected host again", || {
        http_get(port, "/api/v1/hosts").is_some_and(|(status, body)| {
            status == 200 && body.matches("\"connected\"").count() == 1
        })
    })
    .await;

    assert_eq!(
        read_host_id(&server_data).as_deref(),
        Some(host_id.as_str()),
        "a restart must reuse the persisted host id, not enroll a second machine"
    );

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(30), server.wait()).await;
}

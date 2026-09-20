//! The one binary both roles are built from.
//!
//! `loom server` and `loom worker` are two *processes* with one protocol
//! between them (`docs/process-model.md`), and the thing you install is a
//! single file that can be either role. The role is a subcommand, never the
//! invocation name:
//!
//! ```text
//! loom server --bind 127.0.0.1:38886
//! loom worker --server-url http://127.0.0.1:38886 --state /var/lib/loom/host-id
//! ```
//!
//! The two roles are parsed with `clap`; this dispatcher only decides which
//! parser gets the arguments, and answers `--version` before either runs
//! because its exact shape is a release contract
//! ([`loom_server::version_line`]).

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use loom_server::cli::ServerArgs;
use loom_worker::cli::WorkerArgs;

/// The top-level `loom` command: a role, then that role's own flags.
#[derive(Debug, Parser)]
#[command(
    name = "loom",
    about = "loom: the control plane and the execution plane, one binary, two roles.",
    disable_version_flag = true,
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    role: Role,
}

#[derive(Debug, Subcommand)]
enum Role {
    /// The control plane: HTTP API, WebSocket surfaces and the in-process relay.
    Server(ServerArgs),
    /// The execution plane on one machine: connects outbound and runs agents.
    Worker(WorkerArgs),
}

fn main() -> ExitCode {
    let rest: Vec<String> = std::env::args().skip(1).collect();

    // `--version` is answered here, before clap, because the release scripts
    // parse the exact line `version_line` produces; clap's own version flag is
    // disabled on every parser below.
    if rest.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("{}", loom_server::version_line(version_name(&rest)));
        return ExitCode::SUCCESS;
    }

    match Cli::parse().role {
        Role::Server(args) => run_server(args),
        Role::Worker(args) => run_worker(args),
    }
}

fn run_server(args: ServerArgs) -> ExitCode {
    let runtime = match runtime() {
        Ok(runtime) => runtime,
        Err(code) => return code,
    };
    match runtime.block_on(loom_server::run::run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("loom: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_worker(args: WorkerArgs) -> ExitCode {
    let runtime = match runtime() {
        Ok(runtime) => runtime,
        Err(code) => return code,
    };
    match runtime.block_on(loom_worker::run::run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("loom: {error}");
            ExitCode::FAILURE
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, ExitCode> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            eprintln!("loom: could not start the runtime: {error}");
            ExitCode::FAILURE
        })
}

/// The name `--version` prints: the role subcommand when there is one, else
/// `loom`. The subcommand is always the first argument, so the first token is
/// enough.
fn version_name(rest: &[String]) -> &'static str {
    match rest.first().map(String::as_str) {
        Some("server") => "loom-server",
        Some("worker") => "loom-worker",
        _ => "loom",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_line_names_the_role_that_was_asked_for() {
        assert_eq!(version_name(&[]), "loom");
        assert_eq!(version_name(&["--version".into()]), "loom");
        assert_eq!(version_name(&["server".into()]), "loom-server");
        assert_eq!(version_name(&["worker".into()]), "loom-worker");
    }

    #[test]
    fn the_role_parsers_accept_the_flags_a_unit_writes() {
        use clap::Parser;
        let server = ServerArgs::parse_from([
            "loom",
            "--bind",
            "127.0.0.1:9000",
            "--data-dir",
            "/var/lib/loom",
            "--local-worker",
        ]);
        assert_eq!(server.bind.port(), 9000);
        assert!(server.local_worker);

        let worker = WorkerArgs::parse_from([
            "loom",
            "--server-url",
            "http://127.0.0.1:38886",
            "--state",
            "/var/lib/loom/host-id",
            "--no-auto-update",
        ]);
        assert_eq!(worker.server_url, "http://127.0.0.1:38886");
        assert!(worker.no_auto_update);
    }
}

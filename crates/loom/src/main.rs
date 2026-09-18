//! The one binary both roles are built from.
//!
//! `loom-server` and `loom-daemon` are *roles*, not artifacts: they are two
//! processes with one protocol between them (`docs/process-model.md`), and the
//! thing you install is a single file that can be either. Which role it takes is
//! decided by how it was invoked, so every existing path keeps working:
//!
//! ```text
//! loom server            loom daemon --server-url http://127.0.0.1:38886
//! loom-server            loom-daemon --server-url http://127.0.0.1:38886
//! ```
//!
//! The second form is what the installed symlinks give you, and it is why
//! systemd units, the daemon's self-update and anything that spawns a binary by
//! name need no changes. What this does **not** do is merge the two processes:
//! one file can be started twice, as two supervisors expecting different
//! lifetimes, and neither start depends on the other. `install.sh` links the
//! names; nothing links the lifetimes.

use std::path::Path;
use std::process::ExitCode;

/// Which of the two processes this invocation is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Role {
    Server,
    Daemon,
}

fn main() -> ExitCode {
    let mut args = std::env::args();
    let argv0 = args.next().unwrap_or_default();
    let rest: Vec<String> = args.collect();

    let named_by_argv0 = role_from_argv0(&argv0);
    let role = match named_by_argv0 {
        Some(role) => Ok(Some(role)),
        None => role_from_args(&rest),
    };
    let role = match role {
        Ok(Some(role)) => role,
        Ok(None) => {
            print_usage(&argv0);
            return ExitCode::SUCCESS;
        }
        Err(argument) => {
            eprintln!("loom: unknown role: {argument}");
            print_usage(&argv0);
            return ExitCode::from(2);
        }
    };

    // The role word belongs to this dispatcher, not to a role's own parser, so
    // it is consumed here — whether it named the role (`loom daemon …`) or
    // repeats what the invocation name already said (`loom-daemon daemon …`,
    // which a supervisor copying a binary into place can produce).
    let role_word = match role {
        Role::Server => "server",
        Role::Daemon => "daemon",
    };
    let mut role_args: Vec<String> = if named_by_argv0.is_some() {
        rest.clone()
    } else {
        rest.iter().skip(1).cloned().collect()
    };
    if role_args.first().map(String::as_str) == Some(role_word) {
        role_args.remove(0);
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("loom: could not start the runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    // `loom server --help` must not start a server: the role has no flags of
    // its own to document, so it shows the shared usage — the same text a bare
    // `loom` prints. The daemon documents its own flags, so it keeps them.
    if role == Role::Server
        && role_args
            .iter()
            .any(|arg| arg == "-h" || arg == "--help" || arg == "help")
    {
        print_usage(&argv0);
        return ExitCode::SUCCESS;
    }

    let outcome = match role {
        Role::Server => runtime.block_on(loom_server::run::run(&role_args)),
        Role::Daemon => runtime.block_on(loom_daemon::run::run(&role_args)),
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("loom: {error}");
            ExitCode::FAILURE
        }
    }
}

/// The role the binary was invoked under, from its own name.
///
/// A symlink is how one file answers to two names: `loom-server` starts the
/// control plane and `loom-daemon` the execution plane. This is checked before
/// the arguments, so `loom-daemon --server-url …` and `loom daemon
/// --server-url …` are the same command.
fn role_from_argv0(argv0: &str) -> Option<Role> {
    let name = Path::new(argv0)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    // `loom-server` also contains "loom", so the role suffix is what decides.
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    if name.ends_with("loom-server") {
        return Some(Role::Server);
    }
    if name.ends_with("loom-daemon") {
        return Some(Role::Daemon);
    }
    None
}

/// The role named as the first argument, for the `loom <role>` form.
///
/// `None` means "print the usage": no arguments at all is a request for help,
/// not a guess at a role. `Err` carries the argument that was not understood.
fn role_from_args(args: &[String]) -> Result<Option<Role>, String> {
    let Some(first) = args.first() else {
        return Ok(None);
    };
    match first.as_str() {
        "server" => Ok(Some(Role::Server)),
        "daemon" => Ok(Some(Role::Daemon)),
        "-h" | "--help" | "help" | "-v" | "--version" | "version" => Ok(None),
        other => Err(other.to_owned()),
    }
}

fn print_usage(argv0: &str) {
    let name = Path::new(argv0)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "loom".to_owned());
    println!("{}", loom_server::version_line("loom"));
    println!();
    println!("USAGE");
    println!("loom <role> [flags]      ({name} <role> [flags])");
    println!();
    println!("ROLES");
    println!("server: the control plane. Reads LOOM_BIND, LOOM_DATA_DIR, LOOM_NODE_ID,");
    println!("        LOOM_REDIS_URL, LOOM_LOCAL_HOST_ID, LOOM_UI_PROXY,");
    println!("        LOOM_ARTIFACT_DIR and LOOM_GIT_COMMIT from the environment.");
    println!("daemon: the execution plane on one machine. Run `loom daemon --help` for its");
    println!("        flags (--server-url, --name, --state, --provider-cmd, …).");
    println!();
    println!("The two are separate processes with one protocol between them; one role never");
    println!("starts or supervises the other. See docs/process-model.md.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn the_invocation_name_decides_the_role() {
        assert_eq!(
            role_from_argv0("/usr/local/bin/loom-server"),
            Some(Role::Server)
        );
        assert_eq!(role_from_argv0("./loom-daemon"), Some(Role::Daemon));
        // The layout `install.sh` creates: one file, two names.
        assert_eq!(role_from_argv0("loom-server"), Some(Role::Server));
        assert_eq!(role_from_argv0("loom-daemon.exe"), Some(Role::Daemon));
        // The plain name carries no role: the subcommand does.
        assert_eq!(role_from_argv0("/usr/local/bin/loom"), None);
        assert_eq!(role_from_argv0("something-else"), None);
    }

    #[test]
    fn the_first_argument_decides_the_role_when_the_name_does_not() {
        assert_eq!(role_from_args(&args(&["server"])), Ok(Some(Role::Server)));
        assert_eq!(
            role_from_args(&args(&["daemon", "--name", "x"])),
            Ok(Some(Role::Daemon))
        );
        // No argument asks for the usage rather than guessing a role.
        assert_eq!(role_from_args(&args(&[])), Ok(None));
        assert_eq!(role_from_args(&args(&["--help"])), Ok(None));
        assert_eq!(role_from_args(&args(&["--version"])), Ok(None));
        assert_eq!(role_from_args(&args(&["servre"])), Err("servre".to_owned()));
    }

    #[test]
    fn a_name_that_ends_in_a_role_wins_over_an_argument() {
        // A symlink is explicit about the role; the arguments behind it are the
        // role's own, so `loom-daemon server` must not turn into a server.
        assert_eq!(role_from_argv0("loom-daemon"), Some(Role::Daemon));
        assert_eq!(role_from_args(&args(&["server"])), Ok(Some(Role::Server)));
    }
}

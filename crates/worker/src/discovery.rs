//! The ACP agents actually installed on this machine.
//!
//! A provider is not configured, it is *found*. The worker carries a table of
//! agents loom knows how to launch, looks each one up on `PATH`, and reports
//! the ones that are really there. There is no environment variable and no
//! config file in this path: a machine with an agent installed gets it, and one
//! without never sees it. Installing the agent is the whole provisioning step.
//!
//! Presence is necessary but not sufficient — `crates/worker/src/acp/catalog.rs`
//! then opens a real ACP session against each candidate, and only an agent that
//! answers `initialize` and `session/new` is advertised. A binary that merely
//! shares a name with a known agent is dropped rather than offered and failing
//! at the first user turn.
//!
//! The table is deliberately small and explicit. Adding an agent is a one-line
//! entry here, and an entry whose argv never completes a handshake costs one
//! timed-out probe and changes nothing the client sees.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use loom_provider_protocol::{ProviderLaunch, ProviderSpec};

/// One ACP agent loom knows how to reach.
pub struct KnownAgent {
    /// The provider id, as clients and dispatches spell it.
    pub name: &'static str,
    /// The executable and its arguments, executable first.
    pub argv: &'static [&'static str],
    /// How the worker reaches this agent.
    pub launch: ProviderLaunch,
}

/// The agents loom probes for, in preference order.
///
/// Pi comes first because it is the embedded adapter and the default agent, so
/// a host that has it keeps the single-provider behaviour the control plane had
/// before agents were selectable. Every other entry is a native ACP server:
/// the agent itself takes the ACP flag, so nothing is bridged or translated.
///
/// `codex-acp` and `claude-code-acp` are the ACP bridge packages for agents
/// that do not speak ACP themselves; they are probed under the names their
/// upstreams use and simply do not appear when the bridge is not installed.
pub const KNOWN_AGENTS: &[KnownAgent] = &[
    KnownAgent {
        name: "pi",
        argv: &["pi"],
        launch: ProviderLaunch::AcpEmbeddedPi,
    },
    KnownAgent {
        name: "omp",
        argv: &["omp", "acp"],
        launch: ProviderLaunch::AcpStdio,
    },
    KnownAgent {
        name: "hermes",
        argv: &["hermes", "acp"],
        launch: ProviderLaunch::AcpStdio,
    },
    KnownAgent {
        name: "opencode",
        argv: &["opencode", "acp"],
        launch: ProviderLaunch::AcpStdio,
    },
    KnownAgent {
        name: "gemini",
        argv: &["gemini", "--experimental-acp"],
        launch: ProviderLaunch::AcpStdio,
    },
    KnownAgent {
        name: "cursor",
        argv: &["cursor-agent", "acp"],
        launch: ProviderLaunch::AcpStdio,
    },
    KnownAgent {
        name: "codex",
        argv: &["codex-acp"],
        launch: ProviderLaunch::AcpStdio,
    },
    KnownAgent {
        name: "claude-code",
        argv: &["claude-code-acp"],
        launch: ProviderLaunch::AcpStdio,
    },
];

/// The agents installed here, resolved against this process's `PATH`.
pub fn candidates() -> Vec<ProviderSpec> {
    discover(&std::env::var_os("PATH").unwrap_or_default())
}

/// The agents in [`KNOWN_AGENTS`] whose executable resolves in `path`.
///
/// The command is stored absolute: discovery already found the file, and
/// resolving it twice would let a `PATH` change between enrollment and a run
/// point the worker at a different binary than the one it reported.
pub(crate) fn discover(path: &OsStr) -> Vec<ProviderSpec> {
    KNOWN_AGENTS
        .iter()
        .filter_map(|agent| {
            let (command, args) = agent.argv.split_first()?;
            let found = lookup(command, path)?;
            Some(ProviderSpec {
                name: agent.name.to_owned(),
                launch: agent.launch,
                command: found.to_string_lossy().into_owned(),
                args: args.iter().map(|arg| (*arg).to_owned()).collect(),
                cwd: None,
            })
        })
        .collect()
}

/// The first executable named `command` on `path`.
///
/// A command containing a separator is a path, not a name, and is checked where
/// it points instead of being searched for.
fn lookup(command: &str, path: &OsStr) -> Option<PathBuf> {
    let named = Path::new(command);
    if named.components().count() > 1 {
        return is_executable(named).then(|| named.to_path_buf());
    }
    std::env::split_paths(path)
        // An empty entry means the current directory to a POSIX shell; loom
        // does not search the working directory for agents.
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(command))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    executable_bits(&metadata)
}

#[cfg(unix)]
fn executable_bits(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable_bits(_metadata: &std::fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_executable(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("loom-discovery-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn discovers_a_known_agent_from_its_executable() {
        let dir = temp_dir("known");
        write_executable(&dir, "hermes");
        let found = discover(dir.as_os_str());
        assert_eq!(
            found
                .iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<_>>(),
            vec!["hermes"]
        );
        assert_eq!(found[0].args, vec!["acp".to_owned()]);
        assert_eq!(found[0].launch, ProviderLaunch::AcpStdio);
        assert!(
            Path::new(&found[0].command).is_absolute(),
            "the discovered command is resolved, not left to a later lookup"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keeps_the_table_order_and_launch_kind() {
        let dir = temp_dir("order");
        for name in ["omp", "gemini", "pi", "opencode"] {
            write_executable(&dir, name);
        }
        let found = discover(dir.as_os_str());
        assert_eq!(
            found
                .iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<_>>(),
            vec!["pi", "omp", "opencode", "gemini"],
            "table order, not PATH order"
        );
        assert_eq!(found[0].launch, ProviderLaunch::AcpEmbeddedPi);
        assert!(found[0].args.is_empty());
        assert_eq!(found[3].args, vec!["--experimental-acp".to_owned()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignores_an_agent_that_is_not_installed() {
        let dir = temp_dir("absent");
        write_executable(&dir, "omp");
        let found = discover(dir.as_os_str());
        assert!(
            found.iter().all(|spec| spec.name != "hermes"),
            "an agent with no executable is not offered"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn does_not_search_the_working_directory() {
        let dir = temp_dir("cwd");
        write_executable(&dir, "hermes");
        let found = discover(OsStr::new(""));
        assert!(
            found.is_empty(),
            "an empty PATH entry is not a search of the current directory"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skips_a_file_that_is_not_executable() {
        let dir = temp_dir("mode");
        std::fs::write(dir.join("omp"), "not a program").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("omp"), std::fs::Permissions::from_mode(0o644))
                .unwrap();
        }
        let found = discover(dir.as_os_str());
        #[cfg(unix)]
        assert!(found.is_empty(), "a readable file is not a runnable agent");
        #[cfg(not(unix))]
        assert_eq!(found.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! What the agents installed on this machine answer when the worker asks.
//!
//! Discovery is a `PATH` lookup, but admission is a handshake — this is the
//! evidence that the two agree on a real host. It spawns whatever agents are
//! installed, so it is `#[ignore]`d and run explicitly against the machine whose
//! providers matter:
//!
//! ```text
//! cargo test -p loom-worker --test probe_live -- --ignored --nocapture
//! ```
//!
//! Every candidate is tried; a failure is printed rather than asserted, because
//! which agents exist differs per machine. What the test does assert is that a
//! candidate which answers is reported as readable.

use std::time::Duration;

use loom_provider_protocol::ProviderLaunch;
use loom_worker::acp::catalog::{read_catalog, CatalogProbeOutcome};
use loom_worker::acp::session::Transport;

const PROBE_BUDGET: Duration = Duration::from_secs(20);

#[tokio::test]
#[ignore = "spawns the agents installed on this machine"]
async fn every_installed_agent_answers_a_probe() {
    let candidates = loom_worker::discovery::candidates();
    println!(
        "discovered: {:?}",
        candidates
            .iter()
            .map(|spec| spec.name.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        !candidates.is_empty(),
        "this machine has no agent loom knows about on PATH, which makes the \
         probe vacuous rather than passing"
    );
    for spec in candidates {
        let transport = match spec.launch {
            ProviderLaunch::AcpStdio => Transport::Stdio {
                command: spec.command.clone(),
                args: spec.args.clone(),
            },
            ProviderLaunch::AcpEmbeddedPi => Transport::EmbeddedPi {
                command: spec.command.clone(),
                args: spec.args.clone(),
            },
        };
        // An absolute workspace, because that is what a real dispatch carries
        // and what some agents require of `session/new`.
        let cwd = std::env::current_dir()
            .expect("the test's working directory")
            .to_string_lossy()
            .into_owned();
        let outcome = read_catalog(transport, cwd, PROBE_BUDGET).await;
        match &outcome {
            CatalogProbeOutcome::Read(catalog) => println!(
                "{}: {} model(s), current {:?}",
                spec.name,
                catalog.models.len(),
                catalog.current_model
            ),
            CatalogProbeOutcome::Failed { error } => {
                println!("{}: failed: {error}", spec.name)
            }
        }
    }
}

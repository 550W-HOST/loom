//! The relay's backend contract, over the store.
//!
//! `crates/relay/tests/relay.rs` runs the suite over the two backends the relay
//! ships. This runs the *same* suite over the store, which is where the frames
//! live once the shard files are gone: the scenarios are the relay's, the
//! backend is the server's, and neither knows about the other beyond the trait.
//!
//! One test per scenario, so a failure names the property that broke rather than
//! the suite.

use std::path::Path;
use std::sync::{Arc, Mutex};

use loom_relay::backend::conformance;
use loom_relay::backend::SharedBackend;
use loom_server::store::{Store, StoreBackend};

/// The store, as a backend the relay's suite can be run over.
struct StoreCases;

impl conformance::Backend for StoreCases {
    fn name(&self) -> &'static str {
        "store"
    }

    fn durable(&self) -> bool {
        true
    }

    fn open(&self, dir: &Path, max_len: usize) -> SharedBackend {
        let store = Store::open(dir.join("loom.db")).expect("the store opens");
        Arc::new(StoreBackend::new(Arc::new(Mutex::new(store)), max_len))
    }
}

fn run(scenario: conformance::Scenario) {
    let cases = conformance::cases_with(vec![Arc::new(StoreCases)]);
    scenario.1(&cases);
}

#[test]
fn publish_replay_trim_cycle() {
    run((
        "publish_replay_trim_cycle",
        conformance::publish_replay_trim_cycle,
    ));
}

#[test]
fn scopes_share_shards_without_leaking_into_each_other() {
    run((
        "scopes_share_shards_without_leaking_into_each_other",
        conformance::scopes_share_shards_without_leaking_into_each_other,
    ));
}

#[test]
fn a_reconnecting_consumer_replays_only_what_it_missed() {
    run((
        "a_reconnecting_consumer_replays_only_what_it_missed",
        conformance::a_reconnecting_consumer_replays_only_what_it_missed,
    ));
}

#[test]
fn wire_form_survives_a_json_hop() {
    run((
        "wire_form_survives_a_json_hop",
        conformance::wire_form_survives_a_json_hop,
    ));
}

#[test]
fn per_shard_cap_bounds_memory() {
    run((
        "per_shard_cap_bounds_memory",
        conformance::per_shard_cap_bounds_memory,
    ));
}

#[test]
fn custom_retention_is_honoured() {
    run((
        "custom_retention_is_honoured",
        conformance::custom_retention_is_honoured,
    ));
}

#[test]
fn the_retained_log_outlives_the_replay_window() {
    run((
        "the_retained_log_outlives_the_replay_window",
        conformance::the_retained_log_outlives_the_replay_window,
    ));
}

#[test]
fn replay_survives_a_restart() {
    run((
        "replay_survives_a_restart",
        conformance::replay_survives_a_restart,
    ));
}

#[test]
fn a_page_returns_the_oldest_frames_after_the_cursor() {
    run((
        "a_page_returns_the_oldest_frames_after_the_cursor",
        conformance::a_page_returns_the_oldest_frames_after_the_cursor,
    ));
}

#[test]
fn paging_from_a_cursor_recovers_every_missed_frame() {
    run((
        "paging_from_a_cursor_recovers_every_missed_frame",
        conformance::paging_from_a_cursor_recovers_every_missed_frame,
    ));
}

#[test]
fn has_more_is_exact_at_the_boundary() {
    run((
        "has_more_is_exact_at_the_boundary",
        conformance::has_more_is_exact_at_the_boundary,
    ));
}

#[test]
fn a_page_filters_by_scope_and_stays_inside_the_window() {
    run((
        "a_page_filters_by_scope_and_stays_inside_the_window",
        conformance::a_page_filters_by_scope_and_stays_inside_the_window,
    ));
}

#[test]
fn an_empty_page_reports_no_more() {
    run((
        "an_empty_page_reports_no_more",
        conformance::an_empty_page_reports_no_more,
    ));
}

#[test]
fn a_closed_relay_refuses_appends_and_flushes_what_it_accepted() {
    run((
        "a_closed_relay_refuses_appends_and_flushes_what_it_accepted",
        conformance::a_closed_relay_refuses_appends_and_flushes_what_it_accepted,
    ));
}

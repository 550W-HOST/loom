//! End-to-end behaviour of the relay layer through its public API.
//!
//! The scenarios themselves live in `loom_relay::backend::conformance`, because
//! the backend that matters in a deployment is built where the store is: this
//! file runs them over the in-process backend the relay ships, and
//! `crates/server/tests/relay_store_contract.rs` runs the same suite over the
//! store.

use loom_relay::backend::conformance;

fn run(scenario: fn(&[conformance::Case])) {
    scenario(&conformance::cases());
}

#[test]
fn publish_replay_trim_cycle() {
    run(conformance::publish_replay_trim_cycle);
}

#[test]
fn scopes_share_shards_without_leaking_into_each_other() {
    run(conformance::scopes_share_shards_without_leaking_into_each_other);
}

#[test]
fn a_reconnecting_consumer_replays_only_what_it_missed() {
    run(conformance::a_reconnecting_consumer_replays_only_what_it_missed);
}

#[test]
fn wire_form_survives_a_json_hop() {
    run(conformance::wire_form_survives_a_json_hop);
}

#[test]
fn per_shard_cap_bounds_memory() {
    run(conformance::per_shard_cap_bounds_memory);
}

#[test]
fn custom_retention_is_honoured() {
    run(conformance::custom_retention_is_honoured);
}

#[test]
fn the_retained_log_outlives_the_replay_window() {
    run(conformance::the_retained_log_outlives_the_replay_window);
}

#[test]
fn replay_survives_a_restart() {
    run(conformance::replay_survives_a_restart);
}

#[test]
fn a_page_returns_the_oldest_frames_after_the_cursor() {
    run(conformance::a_page_returns_the_oldest_frames_after_the_cursor);
}

#[test]
fn paging_from_a_cursor_recovers_every_missed_frame() {
    run(conformance::paging_from_a_cursor_recovers_every_missed_frame);
}

#[test]
fn has_more_is_exact_at_the_boundary() {
    run(conformance::has_more_is_exact_at_the_boundary);
}

#[test]
fn a_page_filters_by_scope_and_stays_inside_the_window() {
    run(conformance::a_page_filters_by_scope_and_stays_inside_the_window);
}

#[test]
fn an_empty_page_reports_no_more() {
    run(conformance::an_empty_page_reports_no_more);
}

#[test]
fn a_closed_relay_refuses_appends_and_flushes_what_it_accepted() {
    run(conformance::a_closed_relay_refuses_appends_and_flushes_what_it_accepted);
}

//! Active time is run time (spec: Active time).

mod common;

use common::*;
use querymt_agent_session_core::SessionCore;
use querymt_agent_session_core::input::SessionInput;
use querymt_agent_session_core::view::ActiveTime;
use serde_json::{Value, json};

/// A state update; `host_at` is the agent's `atMs`, `client_at` the client's
/// own clock reading.
fn state(state: &str, run: &str, host_at: Option<i64>, client_at: i64) -> SessionInput {
    let mut notification = json!({ "sessionId": "s1", "state": state, "runId": run });
    if state == "idle" {
        notification["stopReason"] = json!("end_turn");
    }
    if let Some(at) = host_at {
        notification["atMs"] = json!(at);
    }
    SessionInput::StateUpdate {
        notification,
        at_ms: client_at,
    }
}

/// A snapshot whose summary carries `active_ms`; `running` is the run the
/// runtime status reports as active, with its host start.
fn snapshot(active_ms: u64, running: Option<(&str, u64)>) -> SessionInput {
    let mut status = runtime("idle");
    if let Some((run, started)) = running {
        status["phase"] = json!("model");
        status["active_run_id"] = json!(run);
        status["run_started_at_ms"] = json!(started);
    }
    loaded(load_response(
        vec![prompt(1, "u1", "hi")],
        json!({
            "runtimeStatus": status,
            "summary": {
                "usage": { "latestRequest": null, "sessionTotals": null, "partial": false,
                           "requestCount": 0,
                           "accounting": serde_json::to_value(
                               querymt_agent_protocol_types::accounting::ScopeTotals::default())
                               .unwrap() },
                "provider": null, "pendingInputs": [], "childSessions": [],
                "activeMs": active_ms
            }
        }),
    ))
}

fn active(core: &SessionCore) -> Option<ActiveTime> {
    core.header().active_time.clone()
}

fn time(total_ms: u64, active_since_ms: Option<i64>) -> Option<ActiveTime> {
    Some(ActiveTime {
        total_ms,
        active_since_ms,
    })
}

#[test]
fn reload_keeps_the_total() {
    let mut core = SessionCore::new("s1");
    let report = core.apply(&[snapshot(120_000, None)]);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(active(&core), time(120_000, None));
    // A second load replaces the total, it does not add to it.
    core.apply(&[snapshot(120_000, None)]);
    assert_eq!(active(&core), time(120_000, None));
}

#[test]
fn live_run_adds_its_time() {
    let mut core = SessionCore::new("s1");
    core.apply(&[snapshot(120_000, None)]);
    core.apply(&[state("running", "r1", Some(10_000), 99)]);
    assert_eq!(active(&core), time(120_000, Some(10_000)));
    core.apply(&[state("idle", "r1", Some(15_000), 99)]);
    assert_eq!(active(&core), time(125_000, None));
}

#[test]
fn load_during_a_run() {
    let mut core = SessionCore::new("s1");
    core.apply(&[snapshot(120_000, Some(("r3", 50_000)))]);
    assert_eq!(active(&core), time(120_000, Some(50_000)));
}

#[test]
fn permission_wait_keeps_the_run_start() {
    let mut core = SessionCore::new("s1");
    core.apply(&[snapshot(0, None)]);
    core.apply(&[state("running", "r1", Some(10_000), 0)]);
    core.apply(&[state("requires_action", "r1", Some(11_000), 0)]);
    assert_eq!(active(&core), time(0, Some(10_000)));
    core.apply(&[state("running", "r1", Some(12_000), 0)]);
    assert_eq!(active(&core), time(0, Some(10_000)));
    core.apply(&[state("idle", "r1", Some(20_000), 0)]);
    assert_eq!(active(&core), time(10_000, None));
}

#[test]
fn load_during_a_run_keeps_the_snapshots_start() {
    let mut core = SessionCore::new("s1");
    core.apply(&[snapshot(0, Some(("r3", 50_000)))]);
    // The current-state sync after a load carries no `atMs`.
    core.apply(&[state("running", "r3", None, 7)]);
    assert_eq!(active(&core), time(0, Some(50_000)));
    core.apply(&[state("idle", "r3", Some(65_000), 7)]);
    assert_eq!(active(&core), time(15_000, None));
}

#[test]
fn client_clock_skew() {
    let mut core = SessionCore::new("s1");
    core.apply(&[snapshot(0, Some(("r3", 50_000)))]);
    // The client's clock is ten minutes behind; only the host time counts.
    core.apply(&[state("idle", "r3", Some(65_000), 5_000)]);
    assert_eq!(active(&core), time(15_000, None));

    // An idle without `atMs` for a host-started run adds nothing.
    let mut core = SessionCore::new("s1");
    core.apply(&[snapshot(0, Some(("r3", 50_000)))]);
    core.apply(&[state("idle", "r3", None, 5_000)]);
    assert_eq!(active(&core), time(0, None));

    // A client-clock start is never paired with a host end.
    let mut core = SessionCore::new("s1");
    core.apply(&[state("running", "r1", None, 1_000)]);
    assert_eq!(active(&core), time(0, Some(1_000)));
    core.apply(&[state("idle", "r1", Some(65_000), 2_000)]);
    assert_eq!(active(&core), time(0, None));

    // Without any host time both ends are the client's.
    let mut core = SessionCore::new("s1");
    core.apply(&[state("running", "r1", None, 1_000)]);
    core.apply(&[state("idle", "r1", None, 4_000)]);
    assert_eq!(active(&core), time(3_000, None));
}

#[test]
fn the_added_time_is_never_negative() {
    let mut core = SessionCore::new("s1");
    core.apply(&[snapshot(5_000, None)]);
    core.apply(&[state("running", "r1", Some(20_000), 0)]);
    core.apply(&[state("idle", "r1", Some(10_000), 0)]);
    assert_eq!(active(&core), time(5_000, None));
}

#[test]
fn a_new_run_restarts_the_clock_and_an_old_idle_is_ignored() {
    let mut core = SessionCore::new("s1");
    core.apply(&[state("running", "r1", Some(10_000), 0)]);
    core.apply(&[state("running", "r2", Some(30_000), 0)]);
    assert_eq!(active(&core), time(0, Some(30_000)));
    // The end of an older run does not end the current one.
    core.apply(&[state("idle", "r1", Some(40_000), 0)]);
    assert_eq!(active(&core), time(0, Some(30_000)));
    core.apply(&[state("idle", "r2", Some(45_000), 0)]);
    assert_eq!(active(&core), time(15_000, None));
}

#[test]
fn runtime_status_starts_the_clock_for_agents_without_state_updates() {
    let status = |phase: &str, run: Option<&str>, started: Option<u64>| {
        let mut status: Value = runtime(phase);
        status["active_run_id"] = json!(run);
        status["run_started_at_ms"] = json!(started);
        SessionInput::RuntimeObserved {
            status: serde_json::from_value(status).expect("status"),
        }
    };
    let mut core = SessionCore::new("s1");
    core.apply(&[snapshot(1_000, None)]);
    core.apply(&[status("model", Some("r1"), Some(50_000))]);
    assert_eq!(active(&core), time(1_000, Some(50_000)));
    // The same run observed again keeps its start.
    core.apply(&[status("tools", Some("r1"), Some(60_000))]);
    assert_eq!(active(&core), time(1_000, Some(50_000)));
    // An idle observation ends the run; it has no host end time to add.
    core.apply(&[status("idle", None, None)]);
    assert_eq!(active(&core), time(1_000, None));
}

#[test]
fn older_history_does_not_change_active_time() {
    let mut core = SessionCore::new("s1");
    core.apply(&[snapshot(120_000, Some(("r3", 50_000)))]);
    let before = active(&core);
    let page = json!({
        "events": [
            event(1, "run_started", json!({ "run_id": "r0", "origin": "interactive" })),
            prompt(2, "u0", "old"),
            event(3, "run_completed", json!({ "run_id": "r0", "outcome": "completed" })),
        ],
        "history": { "complete": true, "before": null },
        "truncated": []
    });
    let report = core.apply(&[SessionInput::HistoryPage { response: page }]);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(active(&core), before);
}

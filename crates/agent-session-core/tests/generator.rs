//! Seeded trace generator (design D7).
#![cfg(feature = "test-support")]

use querymt_agent_session_core::testing::{TraceSpec, generate};

#[test]
fn same_seed_yields_identical_traces() {
    let a = generate(TraceSpec::small(7));
    let b = generate(TraceSpec::small(7));
    assert_eq!(a.events, b.events);
    assert!(!a.events.is_empty());
}

#[test]
fn different_seeds_yield_different_traces() {
    let a = generate(TraceSpec::small(7));
    let b = generate(TraceSpec::small(8));
    assert_ne!(a.events, b.events);
}

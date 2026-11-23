//! Cost of one streamed chunk applied to a long session (informational).

use criterion::{Criterion, criterion_group, criterion_main};
use querymt_agent_session_core::SessionCore;
use querymt_agent_session_core::input::SessionInput;
use querymt_agent_session_core::testing::{TraceSpec, generate, snapshot_response, text_chunk};
use std::hint::black_box;

fn long_session(factor: usize) -> SessionCore {
    let trace = generate(TraceSpec::scaled(2, factor));
    let mut core = SessionCore::new("s1");
    core.apply(&[SessionInput::SnapshotLoaded {
        response: snapshot_response(&trace.events, serde_json::json!({})),
    }]);
    core
}

fn stream_into_long_session(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_into_long_session");
    for (name, factor) in [("1x", 4), ("4x", 16)] {
        let mut core = long_session(factor);
        let chunk = [text_chunk("m-bench", "lorem ")];
        group.bench_function(name, |b| b.iter(|| black_box(core.apply(&chunk))));
    }
    // The same update, then the last turn read by reference as a native
    // client does after each change report (every entry visited, none copied).
    for (name, factor) in [("1x_read_after_update", 4), ("4x_read_after_update", 16)] {
        let mut core = long_session(factor);
        let chunk = [text_chunk("m-bench", "lorem ")];
        group.bench_function(name, |b| {
            b.iter(|| {
                black_box(core.apply(&chunk));
                let revisions = core
                    .turn_order()
                    .last()
                    .and_then(|id| core.turn_ref(id))
                    .map_or(0, |turn| {
                        turn.entries().map(|entry| entry.revision).sum::<u64>()
                    });
                black_box(revisions)
            });
        });
    }
    group.finish();
}

criterion_group!(benches, stream_into_long_session);
criterion_main!(benches);

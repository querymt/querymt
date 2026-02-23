use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use querymt_agent::session::domain::{ForkOrigin, IntentSnapshot};
use querymt_agent::session::store::SessionStore;
use querymt_agent::session::{SessionListFilter, SqliteStorage, ViewStore};
use std::hint::black_box;
use tempfile::TempDir;
use time::OffsetDateTime;
use tokio::runtime::Runtime;

struct Scenario {
    store: SqliteStorage,
    session_count: usize,
    intents_per_session: usize,
    _tmp_dir: TempDir,
}

fn seed_scenario(rt: &Runtime, session_count: usize, intents_per_session: usize) -> Scenario {
    rt.block_on(async {
        let tmp_dir = tempfile::tempdir().expect("create temp dir for session list benchmark");
        let db_path = tmp_dir.path().join("session_list_bench.sqlite3");
        let store = SqliteStorage::connect(db_path)
            .await
            .expect("create benchmark sqlite store");

        let mut root_session_ids: Vec<String> = Vec::new();

        for i in 0..session_count {
            let root_idx = i % 10;
            let cwd = std::path::PathBuf::from(format!("/tmp/bench-workspace-{}", root_idx));

            let parent_session_id = if i > 0 && i % 8 == 0 {
                Some(root_session_ids[root_idx.min(root_session_ids.len() - 1)].clone())
            } else {
                None
            };

            let fork_origin = parent_session_id.as_ref().map(|_| {
                if i % 16 == 0 {
                    ForkOrigin::Delegation
                } else {
                    ForkOrigin::User
                }
            });

            let session = store
                .create_session(
                    Some(format!("session-{i}")),
                    Some(cwd),
                    parent_session_id,
                    fork_origin,
                )
                .await
                .expect("seed session");

            if i < 10 {
                root_session_ids.push(session.public_id.clone());
            }

            for intent_idx in 0..intents_per_session {
                store
                    .create_intent_snapshot(IntentSnapshot {
                        id: 0,
                        session_id: session.id,
                        task_id: None,
                        summary: format!(
                            "session-{i} intent-{intent_idx}: investigate dashboard/session list latency"
                        ),
                        constraints: None,
                        next_step_hint: None,
                        created_at: OffsetDateTime::now_utc(),
                    })
                    .await
                    .expect("seed intent snapshot");
            }
        }

        Scenario {
            store,
            session_count,
            intents_per_session,
            _tmp_dir: tmp_dir,
        }
    })
}

fn bench_session_list_view(c: &mut Criterion) {
    let rt = Runtime::new().expect("create tokio runtime for session list benchmark");

    let scenarios = vec![
        seed_scenario(&rt, 10, 2),
        seed_scenario(&rt, 100, 2),
        seed_scenario(&rt, 500, 3),
    ];

    let mut group = c.benchmark_group("session_list_view");
    group.sample_size(30);
    group.measurement_time(std::time::Duration::from_secs(20));

    for scenario in &scenarios {
        group.throughput(Throughput::Elements(scenario.session_count as u64));

        let parameter = format!(
            "sessions_{}_intents_{}",
            scenario.session_count, scenario.intents_per_session
        );

        let store = scenario.store.clone();
        group.bench_with_input(
            BenchmarkId::new("unfiltered", &parameter),
            &parameter,
            |b, _| {
                let store = store.clone();
                b.to_async(&rt).iter(|| async {
                    let view = store
                        .get_session_list_view(None)
                        .await
                        .expect("build session list view");
                    black_box(view.total_count);
                    black_box(view.groups.len());
                });
            },
        );

        let store = scenario.store.clone();
        group.bench_with_input(
            BenchmarkId::new("limited_50", &parameter),
            &parameter,
            |b, _| {
                let store = store.clone();
                b.to_async(&rt).iter(|| async {
                    let view = store
                        .get_session_list_view(Some(SessionListFilter {
                            filter: None,
                            limit: Some(50),
                            offset: None,
                        }))
                        .await
                        .expect("build limited session list view");
                    black_box(view.total_count);
                    black_box(view.groups.len());
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default()
        .with_plots()
        .nresamples(100_000)
        .warm_up_time(std::time::Duration::from_secs(3));
    targets = bench_session_list_view
);
criterion_main!(benches);

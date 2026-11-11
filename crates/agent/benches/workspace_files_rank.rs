//! Ranking latency over a large synthetic index. Interactive completion
//! debounces at 80 ms, so 100k paths must stay well under that.

use criterion::{Criterion, criterion_group, criterion_main};
use querymt_agent::index::FileIndexEntry;
use querymt_agent::workspace_files::query_index;
use std::hint::black_box;

fn synthetic_index(count: usize) -> Vec<FileIndexEntry> {
    (0..count)
        .map(|i| FileIndexEntry {
            path: format!(
                "crates/pkg{}/src/module{}/sub{}/file_{i}.rs",
                i % 97,
                i % 31,
                i % 7
            ),
            is_dir: false,
        })
        .collect()
}

fn bench_rank(c: &mut Criterion) {
    let files = synthetic_index(100_000);
    let mut group = c.benchmark_group("workspace_files_rank_100k");
    for query in ["", "mod", "file_4242", "pkg12sub"] {
        group.bench_function(format!("query={query:?}"), |b| {
            b.iter(|| query_index("/repo", Some("/repo"), &files, black_box(query), 50));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_rank);
criterion_main!(benches);

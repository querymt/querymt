//! Cost of capturing an undo snapshot through the `SnapshotBackend` trait.
//!
//! Groups: 2 000 small files cold and warm, and one 256 MiB random file new
//! and unchanged.

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use querymt_agent::snapshot::{GitSnapshotBackend, SnapshotBackend};
use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use tokio::runtime::Runtime;

const SMALL_FILES: usize = 2_000;
const BIG_FILE_BYTES: usize = 256 * 1024 * 1024;

/// Deterministic incompressible bytes (xorshift), so the file does not deflate.
/// Duplicated from the crate's `test_utils`, which benches cannot reach.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn write_small_files(dir: &Path) {
    for i in 0..SMALL_FILES {
        let sub = dir.join(format!("d{:02}", i % 40));
        fs::create_dir_all(&sub).expect("create dir");
        fs::write(
            sub.join(format!("f{i}.txt")),
            format!("content of file {i}\n"),
        )
        .expect("write file");
    }
}

/// Age every file so the stat cache trusts it (outside the racy window).
fn backdate(dir: &Path) {
    let old = std::time::SystemTime::now() - Duration::from_secs(3600);
    let mut pending = vec![dir.to_path_buf()];
    while let Some(d) = pending.pop() {
        for entry in fs::read_dir(&d).expect("read dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                fs::File::options()
                    .write(true)
                    .open(&path)
                    .and_then(|f| f.set_modified(old))
                    .expect("backdate");
            }
        }
    }
}

fn bench_snapshot_track(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("snapshot_track");
    group.sample_size(10);

    // 2 000 small files, cold: every iteration starts from an empty store.
    let small = TempDir::new().expect("tempdir");
    write_small_files(small.path());
    backdate(small.path());
    group.bench_function("small_files_2000_cold", |b| {
        b.iter_batched(
            || TempDir::new().expect("snapshot base"),
            |base| {
                let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
                let id = rt.block_on(backend.track(small.path())).expect("track");
                black_box((id, base))
            },
            BatchSize::PerIteration,
        );
    });

    // The same files, warm: the stat cache covers every file.
    let warm_base = TempDir::new().expect("snapshot base");
    let warm = GitSnapshotBackend::new().with_snapshot_base(warm_base.path());
    rt.block_on(warm.track(small.path())).expect("first track");
    group.bench_function("small_files_2000_warm", |b| {
        b.iter(|| black_box(rt.block_on(warm.track(small.path())).expect("track")));
    });

    // One 256 MiB random file, new: every iteration captures it into an empty store.
    let big = TempDir::new().expect("tempdir");
    fs::write(big.path().join("model.bin"), noise(BIG_FILE_BYTES, 42)).expect("write big file");
    backdate(big.path());
    group.bench_function("big_file_256mib_new", |b| {
        b.iter_batched(
            || TempDir::new().expect("snapshot base"),
            |base| {
                let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
                let id = rt.block_on(backend.track(big.path())).expect("track");
                black_box((id, base))
            },
            BatchSize::PerIteration,
        );
    });

    // The same file, unchanged.
    let big_base = TempDir::new().expect("snapshot base");
    let big_backend = GitSnapshotBackend::new().with_snapshot_base(big_base.path());
    rt.block_on(big_backend.track(big.path()))
        .expect("first track");
    group.bench_function("big_file_256mib_unchanged", |b| {
        b.iter(|| black_box(rt.block_on(big_backend.track(big.path())).expect("track")));
    });

    group.finish();
}

criterion_group!(benches, bench_snapshot_track);
criterion_main!(benches);

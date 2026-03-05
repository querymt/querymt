//! Benchmarks for DedupCheckMiddleware and FunctionIndex
//!
//! These benchmarks measure the performance of duplicate code detection
//! at different scales:
//!
//! - **small**: ~50 files, ~200 functions
//! - **medium**: ~500 files, ~2000 functions
//!
//! ## Running
//!
//! ```bash
//! # Run all dedup benchmarks
//! cargo bench -p querymt-agent --bench dedup_check
//!
//! # Run specific benchmark group
//! cargo bench -p querymt-agent --bench dedup_check -- function_index_build
//! cargo bench -p querymt-agent --bench dedup_check -- find_similar
//! cargo bench -p querymt-agent --bench dedup_check -- check_for_duplicates
//! ```

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use querymt_agent::index::file_index::FileIndexConfig;
use querymt_agent::index::function_index::{FunctionIndex, FunctionIndexConfig};
use querymt_agent::index::workspace_actor::WorkspaceIndexActor;
use querymt_agent::index::{DiffPaths, WorkspaceHandle};
use std::path::Path;
use tempfile::TempDir;
use tokio::runtime::Runtime;

/// Scenario configuration for benchmarks
#[derive(Clone)]
struct BenchScenario {
    name: &'static str,
    file_count: usize,
    functions_per_file: usize,
    /// Whether to include some intentionally similar functions
    include_duplicates: bool,
}

const SCENARIOS: &[BenchScenario] = &[
    BenchScenario {
        name: "small_50_files",
        file_count: 50,
        functions_per_file: 4,
        include_duplicates: true,
    },
    BenchScenario {
        name: "medium_500_files",
        file_count: 500,
        functions_per_file: 4,
        include_duplicates: true,
    },
];

/// Generate a workspace with test files containing functions
fn generate_test_workspace(scenario: &BenchScenario) -> TempDir {
    let temp_dir = tempfile::tempdir().expect("create temp dir for benchmark");
    let root = temp_dir.path();

    // Template functions with varying structure
    let function_templates = [
        // Template 1: Loop with accumulator
        |i: usize, j: usize| {
            format!(
                r#"
pub fn process_items_{i}_{j}(items: &[Item]) -> i32 {{
    let mut result = 0;
    for item in items {{
        let value = item.value;
        if value > 0 {{
            result += value;
        }} else {{
            result -= value.abs();
        }}
    }}
    result
}}
"#
            )
        },
        // Template 2: Match expression
        |i: usize, j: usize| {
            format!(
                r#"
pub fn handle_variant_{i}_{j}(input: &str) -> Option<String> {{
    match input {{
        "alpha" => Some("first".to_string()),
        "beta" => Some("second".to_string()),
        "gamma" => Some("third".to_string()),
        "delta" => Some("fourth".to_string()),
        _ => None,
    }}
}}
"#
            )
        },
        // Template 3: Error handling
        |i: usize, j: usize| {
            format!(
                r#"
pub fn validate_input_{i}_{j}(value: &str) -> Result<(), String> {{
    if value.is_empty() {{
        return Err("empty".to_string());
    }}
    if value.len() > 100 {{
        return Err("too long".to_string());
    }}
    if !value.chars().all(|c| c.is_alphanumeric()) {{
        return Err("invalid chars".to_string());
    }}
    Ok(())
}}
"#
            )
        },
        // Template 4: Builder pattern
        |i: usize, j: usize| {
            format!(
                r#"
pub fn build_config_{i}_{j}(name: &str, value: i32) -> Config {{
    let mut config = Config::default();
    config.name = name.to_string();
    config.value = value;
    config.enabled = true;
    config.timeout = 30;
    config
}}
"#
            )
        },
    ];

    // Duplicate template - will be copied to multiple files for duplicate detection testing
    let duplicate_template = r#"
pub fn u32_from_usize_SUFFIX(value: usize, field: &str, session: Option<&str>) -> u32 {
    u32::try_from(value).unwrap_or_else(|_| {
        log::warn!(
            "{}={} exceeds u32 max (session: {:?})",
            field,
            value,
            session
        );
        u32::MAX
    })
}
"#;

    for i in 0..scenario.file_count {
        let mut content = String::new();

        for j in 0..scenario.functions_per_file {
            let template_idx = (i + j) % function_templates.len();
            content.push_str(&function_templates[template_idx](i, j));
        }

        // Add a duplicate function to some files (for duplicate detection testing)
        if scenario.include_duplicates && i % 10 == 0 {
            let suffix = format!("_{}", i);
            content.push_str(&duplicate_template.replace("SUFFIX", &suffix));
        }

        let filename = format!("module_{:04}.rs", i);
        std::fs::write(root.join(&filename), &content).expect("write benchmark file");
    }

    temp_dir
}

/// Build a FunctionIndex for benchmarking
fn build_index_sync(rt: &Runtime, root: &Path, config: FunctionIndexConfig) -> FunctionIndex {
    rt.block_on(async {
        FunctionIndex::build(root, config)
            .await
            .expect("build index")
    })
}

/// Build a WorkspaceHandle for benchmarking
fn build_workspace_handle_sync(rt: &Runtime, root: &Path) -> WorkspaceHandle {
    rt.block_on(async {
        WorkspaceIndexActor::create(
            root.to_path_buf(),
            FileIndexConfig::default(),
            FunctionIndexConfig::default(),
        )
        .await
        .expect("create workspace handle")
    })
}

/// Benchmark: FunctionIndex::build
fn bench_function_index_build(c: &mut Criterion) {
    let rt = Runtime::new().expect("create tokio runtime");

    let mut group = c.benchmark_group("function_index_build");
    group.sample_size(10); // Fewer samples since indexing is slow
    group.measurement_time(std::time::Duration::from_secs(30));

    for scenario in SCENARIOS {
        let temp_dir = generate_test_workspace(scenario);
        let root = temp_dir.path().to_path_buf();

        let total_functions = scenario.file_count * scenario.functions_per_file;
        group.throughput(Throughput::Elements(total_functions as u64));

        group.bench_with_input(
            BenchmarkId::new("build", scenario.name),
            &scenario.name,
            |b, _| {
                b.to_async(&rt).iter(|| async {
                    let config = FunctionIndexConfig::default();
                    let _ = FunctionIndex::build(&root, config)
                        .await
                        .expect("build index");
                });
            },
        );
    }

    group.finish();
}

/// Benchmark: FunctionIndex::find_similar_to_code
fn bench_find_similar(c: &mut Criterion) {
    let rt = Runtime::new().expect("create tokio runtime");

    // Probe code - structurally similar to the duplicate template
    let probe_code = r#"
pub fn convert_to_u32(val: usize, name: &str, sess: Option<&str>) -> u32 {
    u32::try_from(val).unwrap_or_else(|_| {
        log::warn!(
            "{}={} exceeds u32 max (session: {:?})",
            name,
            val,
            sess
        );
        u32::MAX
    })
}
"#;

    let mut group = c.benchmark_group("find_similar_to_code");
    group.sample_size(20);
    group.measurement_time(std::time::Duration::from_secs(20));

    for scenario in SCENARIOS {
        let temp_dir = generate_test_workspace(scenario);
        let config = FunctionIndexConfig::default().with_threshold(0.7);
        let index = build_index_sync(&rt, temp_dir.path(), config);

        group.throughput(Throughput::Elements(1));

        // Benchmark: single probe
        group.bench_with_input(
            BenchmarkId::new("single_probe", scenario.name),
            &scenario.name,
            |b, _| {
                b.iter(|| {
                    let _ = index.find_similar_to_code(Path::new("probe.rs"), probe_code);
                });
            },
        );

        // Benchmark: batch of 10 probes
        group.throughput(Throughput::Elements(10));
        group.bench_with_input(
            BenchmarkId::new("batch_10_probes", scenario.name),
            &scenario.name,
            |b, _| {
                b.iter(|| {
                    for i in 0..10 {
                        let filename = format!("probe_{}.rs", i);
                        let _ = index.find_similar_to_code(Path::new(&filename), probe_code);
                    }
                });
            },
        );
    }

    group.finish();
}

/// Benchmark: Simulating the middleware's check_for_duplicates flow
///
/// This measures the end-to-end cost of checking changed files against
/// an existing index, similar to what DedupCheckMiddleware does.
fn bench_check_for_duplicates(c: &mut Criterion) {
    let rt = Runtime::new().expect("create tokio runtime");

    // New code that will be "written" and checked for duplicates
    let new_file_content = r#"
pub fn new_convert_to_u32(val: usize, name: &str, sess: Option<&str>) -> u32 {
    u32::try_from(val).unwrap_or_else(|_| {
        log::warn!(
            "{}={} exceeds u32 max (session: {:?})",
            name,
            val,
            sess
        );
        u32::MAX
    })
}

pub fn process_batch(items: &[Item]) -> i32 {
    let mut result = 0;
    for item in items {
        let value = item.value;
        if value > 0 {
            result += value;
        } else {
            result -= value.abs();
        }
    }
    result
}
"#;

    let mut group = c.benchmark_group("check_for_duplicates");
    group.sample_size(20);
    group.measurement_time(std::time::Duration::from_secs(20));

    for scenario in SCENARIOS {
        let temp_dir = generate_test_workspace(scenario);

        // Create workspace handle
        let workspace_handle = build_workspace_handle_sync(&rt, temp_dir.path());

        // Write a new file to check
        let new_file_path = temp_dir.path().join("new_module.rs");
        std::fs::write(&new_file_path, new_file_content).expect("write new file");

        // Benchmark: check 1 changed file
        group.throughput(Throughput::Elements(1));
        let new_file_path_clone = new_file_path.clone();
        group.bench_with_input(
            BenchmarkId::new("1_changed_file", scenario.name),
            &scenario.name,
            |b, _| {
                b.to_async(&rt).iter(|| async {
                    let changed_paths = DiffPaths {
                        added: vec![new_file_path_clone.clone()],
                        modified: vec![],
                        removed: vec![],
                    };

                    // Simulate what the middleware does
                    for file_path in changed_paths.changed_files() {
                        if let Ok(source) = std::fs::read_to_string(file_path) {
                            let results = workspace_handle
                                .actor
                                .ask(querymt_agent::index::workspace_actor::FindSimilarToCode {
                                    file_path: file_path.to_path_buf(),
                                    source,
                                })
                                .await
                                .unwrap_or_default();
                            std::hint::black_box(results);
                        }
                    }
                });
            },
        );

        // Create additional new files for batch testing
        let mut new_file_paths = vec![new_file_path];
        for i in 1..5 {
            let path = temp_dir.path().join(format!("new_module_{}.rs", i));
            std::fs::write(&path, new_file_content).expect("write new file");
            new_file_paths.push(path);
        }

        // Benchmark: check 5 changed files
        group.throughput(Throughput::Elements(5));
        group.bench_with_input(
            BenchmarkId::new("5_changed_files", scenario.name),
            &scenario.name,
            |b, _| {
                let paths = new_file_paths.clone();
                b.to_async(&rt).iter(|| async {
                    let changed_paths = DiffPaths {
                        added: paths.clone(),
                        modified: vec![],
                        removed: vec![],
                    };

                    for file_path in changed_paths.changed_files() {
                        if let Ok(source) = std::fs::read_to_string(file_path) {
                            let results = workspace_handle
                                .actor
                                .ask(querymt_agent::index::workspace_actor::FindSimilarToCode {
                                    file_path: file_path.to_path_buf(),
                                    source,
                                })
                                .await
                                .unwrap_or_default();
                            std::hint::black_box(results);
                        }
                    }
                });
            },
        );
    }

    group.finish();
}

/// Benchmark: FunctionIndex with different threshold configurations
fn bench_threshold_configurations(c: &mut Criterion) {
    let rt = Runtime::new().expect("create tokio runtime");

    // Use small scenario for threshold testing
    let scenario = &SCENARIOS[0];
    let temp_dir = generate_test_workspace(scenario);

    let probe_code = r#"
pub fn convert_to_u32(val: usize, name: &str, sess: Option<&str>) -> u32 {
    u32::try_from(val).unwrap_or_else(|_| {
        log::warn!(
            "{}={} exceeds u32 max (session: {:?})",
            name,
            val,
            sess
        );
        u32::MAX
    })
}
"#;

    let thresholds = [0.70, 0.80, 0.85, 0.90, 0.95];

    let mut group = c.benchmark_group("threshold_configurations");
    group.sample_size(20);
    group.measurement_time(std::time::Duration::from_secs(15));

    for threshold in thresholds {
        let config = FunctionIndexConfig::default().with_threshold(threshold);
        let index = build_index_sync(&rt, temp_dir.path(), config);

        group.bench_with_input(
            BenchmarkId::new("find_similar", format!("threshold_{:.2}", threshold)),
            &threshold,
            |b, _| {
                b.iter(|| {
                    let _ = index.find_similar_to_code(Path::new("probe.rs"), probe_code);
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
        .warm_up_time(std::time::Duration::from_secs(3));
    targets =
        bench_function_index_build,
        bench_find_similar,
        bench_check_for_duplicates,
        bench_threshold_configurations
);
criterion_main!(benches);

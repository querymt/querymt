use agent_client_protocol::NewSessionRequest;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use querymt_agent::agent::AgentHandle;
use querymt_agent::config::ConfigSource;
use querymt_agent::runner::{AgentRunner, from_config};
use querymt_agent::send_agent::SendAgent;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::runtime::Runtime;

const EMBEDDED_CONFIG: &str = include_str!("../examples/confs/single_coder.toml");
const EMBEDDED_PROMPT: &str = include_str!("../examples/prompts/default_system.txt");
const EMBEDDED_PROMPT_REF: &str = r#"{ file = "../prompts/default_system.txt" }"#;

#[derive(Clone)]
struct ProviderCase {
    provider: &'static str,
    model: &'static str,
}

impl ProviderCase {
    fn id(&self) -> String {
        format!("{}:{}", self.provider, self.model)
    }
}

struct BenchCase {
    provider: ProviderCase,
    inline_config: String,
    file_config_path: std::path::PathBuf,
    _temp_dir: TempDir,
}

fn provider_cases_from_env() -> Vec<ProviderCase> {
    let default = "anthropic".to_string();
    let raw = std::env::var("QMT_BENCH_PROVIDERS").unwrap_or(default);

    raw.split(',')
        .filter_map(|s| {
            let name = s.trim();
            if name.is_empty() {
                return None;
            }
            let model = match name {
                "anthropic" => "claude-sonnet-4-5-20250929",
                "openai" => "gpt-4o-mini",
                "ollama" => "llama3.1:8b",
                "codex" => "gpt-5-codex",
                _ => return None,
            };
            Some(ProviderCase {
                provider: Box::leak(name.to_string().into_boxed_str()),
                model,
            })
        })
        .collect()
}

fn make_bench_config(provider: &str, model: &str) -> String {
    let inline_prompt = format!("'''{}'''", EMBEDDED_PROMPT);
    let with_prompt = EMBEDDED_CONFIG.replace(EMBEDDED_PROMPT_REF, &inline_prompt);
    let without_mcp = with_prompt
        .split("\n[[mcp]]")
        .next()
        .unwrap_or(&with_prompt)
        .to_string();

    without_mcp
        .replace(
            "provider = \"anthropic\"",
            &format!("provider = \"{}\"", provider),
        )
        .replace(
            "model = \"claude-sonnet-4-5-20250929\"",
            &format!("model = \"{}\"", model),
        )
        .replace("db = \"/tmp/agent2.db\"", "db = \":memory:\"")
}

fn extract_handle(runner: &AgentRunner) -> Arc<AgentHandle> {
    match runner {
        AgentRunner::Single(agent) => agent.handle(),
        AgentRunner::Multi(_) => panic!("expected single-agent config in benchmark"),
    }
}

fn create_bench_cases(rt: &Runtime) -> Vec<BenchCase> {
    let mut cases = Vec::new();

    for provider in provider_cases_from_env() {
        let inline_config = make_bench_config(provider.provider, provider.model);
        let temp_dir = tempfile::tempdir().expect("create temp dir for benchmark config");
        let file_config_path = temp_dir
            .path()
            .join(format!("single_coder_{}.toml", provider.provider));
        std::fs::write(&file_config_path, &inline_config).expect("write benchmark config file");

        let startup_result = rt.block_on(from_config(ConfigSource::Toml(inline_config.clone())));
        match startup_result {
            Ok(_) => cases.push(BenchCase {
                provider,
                inline_config,
                file_config_path,
                _temp_dir: temp_dir,
            }),
            Err(e) => {
                eprintln!(
                    "Skipping provider '{}' (startup preflight failed): {e}",
                    provider.provider
                );
            }
        }
    }

    cases
}

fn bench_agent_startup_and_sessions(c: &mut Criterion) {
    let rt = Runtime::new().expect("create tokio runtime for benchmarks");
    let bench_cases = create_bench_cases(&rt);

    if bench_cases.is_empty() {
        panic!(
            "No provider benchmark cases available. Set QMT_BENCH_PROVIDERS to installed providers."
        );
    }

    let mut startup_group = c.benchmark_group("agent_startup");
    startup_group.sample_size(20);
    startup_group.measurement_time(std::time::Duration::from_secs(20));

    for case in &bench_cases {
        let provider_id = case.provider.id();
        let inline_config = case.inline_config.clone();
        startup_group.bench_with_input(
            BenchmarkId::new("from_config_inline", &provider_id),
            &provider_id,
            |b, _| {
                b.to_async(&rt).iter(|| async {
                    let _ = from_config(ConfigSource::Toml(inline_config.clone()))
                        .await
                        .expect("startup from inline config");
                });
            },
        );

        let file_path = case.file_config_path.clone();
        startup_group.bench_with_input(
            BenchmarkId::new("from_config_file", &provider_id),
            &provider_id,
            |b, _| {
                b.to_async(&rt).iter(|| async {
                    let _ = from_config(file_path.clone())
                        .await
                        .expect("startup from file config");
                });
            },
        );
    }
    startup_group.finish();

    let mut session_group = c.benchmark_group("agent_session_create");
    session_group.sample_size(30);
    session_group.measurement_time(std::time::Duration::from_secs(20));
    session_group.throughput(Throughput::Elements(1));

    let cwd_dir = tempfile::tempdir().expect("create cwd tempdir for session benchmark");
    let cwd = cwd_dir.path().to_path_buf();

    for case in &bench_cases {
        let provider_id = case.provider.id();
        let warm_runner = rt
            .block_on(from_config(ConfigSource::Toml(case.inline_config.clone())))
            .expect("prepare warm runner for session benchmark");
        let warm_handle = extract_handle(&warm_runner);

        session_group.bench_with_input(
            BenchmarkId::new("new_session_warm_runner", &provider_id),
            &provider_id,
            |b, _| {
                b.to_async(&rt).iter(|| async {
                    let req = NewSessionRequest::new(cwd.clone());
                    let _ = warm_handle
                        .new_session(req)
                        .await
                        .expect("create new session");
                });
            },
        );

        let inline_config = case.inline_config.clone();
        session_group.bench_with_input(
            BenchmarkId::new("startup_plus_first_session", &provider_id),
            &provider_id,
            |b, _| {
                b.to_async(&rt).iter(|| async {
                    let runner = from_config(ConfigSource::Toml(inline_config.clone()))
                        .await
                        .expect("startup from inline config");
                    let handle = extract_handle(&runner);
                    let req = NewSessionRequest::new(cwd.clone());
                    let _ = handle.new_session(req).await.expect("create first session");
                });
            },
        );
    }
    session_group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default()
        .with_plots()
        .nresamples(100_000)
        .warm_up_time(std::time::Duration::from_secs(3));
    targets = bench_agent_startup_and_sessions
);
criterion_main!(benches);

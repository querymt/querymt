//! Opt-in diagnostic against a locally built CLI and the configured Codex plugin.
//! Never runs as part of ordinary tests; requests consume account quota.
#![cfg(all(feature = "api", not(target_arch = "wasm32")))]

#[test]
#[ignore = "live Codex inference: requires explicit opt-in, CLI/plugin builds and OAuth; consumes quota"]
fn live_codex_cache_usage() {
    use std::process::{Command, Stdio};
    assert_eq!(
        std::env::var("QMT_RUN_LIVE_CODEX").as_deref(),
        Ok("1"),
        "Set QMT_RUN_LIVE_CODEX=1 to authorize live requests"
    );
    let binary = std::env::var("QMT_TEST_QMT_BIN")
        .expect("QMT_TEST_QMT_BIN must point to the built qmt executable");
    let config = std::env::var("QMT_TEST_PROVIDER_CONFIG").expect(
        "QMT_TEST_PROVIDER_CONFIG must point to providers.toml using the desired Codex plugin",
    );
    let model = std::env::var("QMT_TEST_CODEX_MODEL").unwrap_or_else(|_| "gpt-5.6-sol".into());
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let key = format!("live-cache-{nonce}");
    let mut system = format!("Fixed fixture for cache probe {nonce}.\n");
    for index in 0..400 {
        use std::fmt::Write;
        writeln!(system, "Reference {index:04}: maintain stable configuration, preserve message ordering, and compare server-reported token usage.").unwrap();
    }
    let extra = serde_json::json!({"prompt_cache_key": key});
    for attempt in 1..=3 {
        let output = Command::new(&binary)
            .args([
                "--provider-config",
                &config,
                "-p",
                "codex",
                "--model",
                &model,
            ])
            .args([
                "-s",
                &system,
                "-o",
                "stream=true",
                "-o",
                "timeout_seconds=60",
            ])
            .args(["-o", &format!("extra_body={extra}"), "Reply with OK only."])
            .env("QMT_NO_TELEMETRY", "1")
            .env("RUST_LOG", "warn,qmt_codex=debug")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .output()
            .expect("start qmt live probe");
        // Print only allowlisted diagnostic JSON, never full CLI output or credentials.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut usage_seen = false;
        for line in stdout.lines().chain(stderr.lines()) {
            if let Some((_, payload)) =
                line.split_once("cache response terminal=response.completed usage=")
            {
                let mut parsed =
                    serde_json::Deserializer::from_str(payload).into_iter::<serde_json::Value>();
                if let Some(Ok(usage)) = parsed.next() {
                    eprintln!("attempt={attempt} model={model} usage={usage}");
                    usage_seen = usage["usage_state"]["state"] == "object";
                }
            }
        }
        assert!(
            output.status.success(),
            "Codex probe attempt {attempt} failed; reproduce with qmt_codex=debug for provider errors"
        );
        assert!(
            usage_seen,
            "No raw terminal usage: check that providers.toml loads the patched Codex plugin"
        );
        // Cache misses are diagnostic outcomes, not test failures.
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

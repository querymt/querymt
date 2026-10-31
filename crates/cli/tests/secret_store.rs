#![cfg(target_os = "linux")]

use std::{
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

#[test]
fn unavailable_store_does_not_block_provider_listing_but_secret_commands_fail() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("providers.toml");
    std::fs::write(&config, "providers = []\n").unwrap();
    let run = |args: &[&str]| -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_qmt"))
            .args(["--provider-config", config.to_str().unwrap()])
            .args(args)
            .env("QMT_CONFIG_DIR", root.path())
            .env("QMT_NO_TELEMETRY", "1")
            .env("QMT_SECRET_STORE", "secret-service")
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}/missing-bus", root.path().display()),
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("storage-independent command did not finish");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        child.wait_with_output().unwrap()
    };
    let listed = run(&["--api-key", "synthetic-test-key", "providers"]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let secret = run(&["secrets", "get", "nonexistent-test-key"]);
    assert!(!secret.status.success());
}

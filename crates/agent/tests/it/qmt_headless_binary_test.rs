//! The `qmt-headless` example binary builds with the facade's test build and
//! honours its CLI and exit-status contract. Cargo builds examples for
//! `cargo test`/nextest, so the executable sits next to `deps/`.

use std::path::PathBuf;
use std::process::{Command, Output};

fn headless_bin() -> PathBuf {
    // With a separate cargo build-dir the test executable is not next to the
    // examples, so look in every ancestor for `examples/qmt-headless`.
    let exe = std::env::current_exe().expect("test exe path");
    let name = format!("qmt-headless{}", std::env::consts::EXE_SUFFIX);
    exe.ancestors()
        .map(|dir| dir.join("examples").join(&name))
        .find(|bin| bin.is_file())
        .unwrap_or_else(|| {
            panic!(
                "qmt-headless example binary was not built (run `cargo build -p querymt-agent \
                 --example qmt-headless`); searched above {}",
                exe.display()
            )
        })
}

fn run(args: &[&std::ffi::OsStr], home: &std::path::Path) -> Output {
    Command::new(headless_bin())
        .args(args)
        .env("HOME", home)
        .env("QMT_HOME", home)
        .env("QMT_CONFIG_DIR", home)
        .output()
        .expect("spawn qmt-headless")
}

#[test]
fn headless_binary_builds_and_rejects_bad_usage() {
    let home = tempfile::tempdir().unwrap();

    let out = run(&[], home.path());
    assert_eq!(out.status.code(), Some(2), "{out:?}");

    let out = run(&["run".as_ref(), "profile.toml".as_ref()], home.path());
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--cwd"), "{stderr}");

    let out = run(&["frobnicate".as_ref()], home.path());
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}

#[test]
fn export_of_a_missing_database_exits_nonzero_and_creates_nothing() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let db = work.path().join("missing.db");
    let atif = work.path().join("atif.json");
    let summary = work.path().join("summary.json");

    let out = run(
        &[
            "export".as_ref(),
            "--db".as_ref(),
            db.as_os_str(),
            "--atif".as_ref(),
            atif.as_os_str(),
            "--summary".as_ref(),
            summary.as_os_str(),
        ],
        home.path(),
    );

    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("does not exist"), "{stderr}");
    assert!(!db.exists(), "export must not create the database");
    assert!(!atif.exists());
    assert!(!summary.exists());
}

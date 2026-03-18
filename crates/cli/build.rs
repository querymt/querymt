use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=QMT_VERSION");

    let version = std::env::var("QMT_VERSION").unwrap_or_else(|_| git_describe_or_pkg_version());
    let normalized = version.strip_prefix('v').unwrap_or(&version);

    println!("cargo:rustc-env=QMT_BUILD_VERSION={normalized}");
}

fn git_describe_or_pkg_version() -> String {
    Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string())
        })
}

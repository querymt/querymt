use std::{path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=QMCP_GIT_SHA");
    // This fallback also covers source archives without Git metadata.
    println!("cargo:rerun-if-changed=build.rs");
    for name in ["HEAD", "refs/heads", "packed-refs"] {
        if let Some(path) = git_path(name) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    let sha = std::env::var("QMCP_GIT_SHA")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(git_short_sha)
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=QMCP_GIT_SHA={sha}");
}

fn git_path(name: &str) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-path", name])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    path.exists().then_some(path)
}

fn git_short_sha() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?;
    let sha = sha.trim();
    if sha.is_empty() {
        None
    } else {
        Some(sha.to_string())
    }
}

#[cfg(feature = "dashboard")]
#[path = "build_support/embedded_ui.rs"]
mod embedded_ui;

#[cfg(feature = "dashboard")]
fn pinned_ui_revision() -> &'static str {
    include_str!("embedded-ui-revision").trim()
}

fn main() {
    emit_build_version();

    #[cfg(feature = "dashboard")]
    prepare_dashboard();
}

fn emit_build_version() {
    println!("cargo:rerun-if-env-changed=QMT_VERSION");

    let version = std::env::var("QMT_VERSION").unwrap_or_else(|_| git_describe_or_pkg_version());
    let normalized = version.strip_prefix('v').unwrap_or(&version);

    println!("cargo:rustc-env=QMT_BUILD_VERSION={normalized}");
}

fn git_describe_or_pkg_version() -> String {
    std::process::Command::new("git")
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

#[cfg(feature = "dashboard")]
fn prepare_dashboard() {
    println!("cargo:rerun-if-changed=embedded-ui-revision");
    println!("cargo:rerun-if-env-changed=QMT_UI_DIST");
    println!("cargo:rerun-if-env-changed=QMT_UI_REVISION");

    let out_dir =
        std::env::var_os("OUT_DIR").unwrap_or_else(|| panic!("Cargo did not set OUT_DIR"));
    let ui_dist = std::env::var_os("QMT_UI_DIST");
    let revision = if ui_dist.is_some() {
        None
    } else {
        match std::env::var("QMT_UI_REVISION") {
            Ok(revision) => Some(revision),
            Err(std::env::VarError::NotUnicode(_)) => {
                panic!("QMT_UI_REVISION must be valid UTF-8")
            }
            Err(std::env::VarError::NotPresent) => None,
        }
    };
    let prepared = embedded_ui::prepare_ui(
        std::path::Path::new(&out_dir),
        ui_dist.as_deref(),
        revision.as_deref(),
        pinned_ui_revision(),
    )
    .unwrap_or_else(|err| panic!("failed to prepare embedded UI: {err}"));

    for path in prepared.watched_paths {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!(
        "cargo:rustc-env=QMT_UI_EMBED_DIR={}",
        prepared.embed_dir.display()
    );
    println!("cargo:warning={}", prepared.source_log);
}

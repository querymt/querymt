#[cfg(feature = "dashboard")]
use std::fs;
#[cfg(feature = "dashboard")]
use std::path::Path;
#[cfg(feature = "dashboard")]
use std::process::Command;

fn main() {
    emit_build_version();

    #[cfg(feature = "dashboard")]
    build_ui();
    #[cfg(feature = "dashboard-ng")]
    prepare_dashboard_ng();
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
fn get_package_manager() -> &'static str {
    if Command::new("bun").arg("--version").output().is_ok() {
        println!("cargo:warning=Detected bun - using it for UI build");
        "bun"
    } else if Command::new("npm").arg("--version").output().is_ok() {
        println!("cargo:warning=Detected npm - using it for UI build");
        "npm"
    } else {
        panic!("No package manager found. Please install bun (recommended) or npm.");
    }
}

#[cfg(feature = "dashboard")]
fn build_ui() {
    let ui_dir = Path::new("ui");

    // Rebuild if UI source files change
    println!("cargo:rerun-if-changed=ui/src");
    println!("cargo:rerun-if-changed=ui/package.json");
    println!("cargo:rerun-if-changed=ui/package-lock.json");
    println!("cargo:rerun-if-changed=ui/index.html");
    println!("cargo:rerun-if-changed=ui/vite.config.ts");
    println!("cargo:rerun-if-changed=ui/tsconfig.json");

    println!("cargo:rerun-if-env-changed=QMT_UI_DIST");
    if let Ok(dist_path) = std::env::var("QMT_UI_DIST") {
        let dist_src = Path::new(&dist_path);
        if !dist_src.exists() {
            panic!("QMT_UI_DIST does not exist: {}", dist_path);
        }

        let dist_dst = ui_dir.join("dist");
        if dist_dst.exists() {
            fs::remove_dir_all(&dist_dst)
                .unwrap_or_else(|err| panic!("Failed to remove existing dist: {err}"));
        }
        copy_dir_all(dist_src, &dist_dst)
            .unwrap_or_else(|err| panic!("Failed to copy UI dist: {err}"));
        println!("cargo:warning=Using prebuilt UI from QMT_UI_DIST");
        return;
    }

    let pm = get_package_manager();

    // Run install if node_modules doesn't exist or package.json changed
    if !ui_dir.join("node_modules").exists() {
        println!(
            "cargo:warning=Running {} install for UI dependencies...",
            pm
        );
        let status = Command::new(pm)
            .args(["install"])
            .current_dir(ui_dir)
            .status()
            .unwrap_or_else(|_| panic!("Failed to run {} install", pm));

        if !status.success() {
            panic!("{} install failed", pm);
        }
    }

    // Run build
    println!("cargo:warning=Building UI with {}...", pm);
    let status = Command::new(pm)
        .args(["run", "build"])
        .current_dir(ui_dir)
        .status()
        .unwrap_or_else(|_| panic!("Failed to run {} build", pm));

    if !status.success() {
        panic!("{} build failed", pm);
    }

    // Verify dist directory was created
    if !ui_dir.join("dist").exists() {
        panic!("UI build succeeded but dist/ directory not found");
    }

    println!("cargo:warning=UI build complete!");
}

#[cfg(feature = "dashboard")]
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(feature = "dashboard-ng")]
fn prepare_dashboard_ng() {
    const ENV_NAME: &str = "QMT_DASHBOARD_NG_DIST";
    println!("cargo:rerun-if-env-changed={ENV_NAME}");

    let dist_path = std::env::var(ENV_NAME)
        .unwrap_or_else(|_| panic!("{ENV_NAME} must point to a prebuilt embedded dashboard"));
    let dist_src = Path::new(&dist_path);
    if !dist_src.is_dir() {
        panic!("{ENV_NAME} is not a directory: {dist_path}");
    }
    validate_dashboard_ng_manifest(dist_src);

    let dist_dst = Path::new("ui/dashboard-ng-dist");
    if dist_dst.exists() {
        fs::remove_dir_all(dist_dst)
            .unwrap_or_else(|err| panic!("Failed to remove existing dashboard-ng assets: {err}"));
    }
    copy_dir_all(dist_src, dist_dst)
        .unwrap_or_else(|err| panic!("Failed to copy dashboard-ng assets: {err}"));
    println!("cargo:warning=Using prebuilt dashboard-ng from {ENV_NAME}");
}

#[cfg(feature = "dashboard-ng")]
fn validate_dashboard_ng_manifest(dist: &Path) {
    if !dist.join("index.html").is_file() {
        panic!("QMT_DASHBOARD_NG_DIST must contain index.html");
    }

    let manifest_path = dist.join("querymt-ui.json");
    let manifest = fs::read_to_string(&manifest_path)
        .unwrap_or_else(|err| panic!("Failed to read {}: {err}", manifest_path.display()));
    let manifest: serde_json::Value = serde_json::from_str(&manifest)
        .unwrap_or_else(|err| panic!("Invalid {}: {err}", manifest_path.display()));

    let target = manifest.get("target").and_then(serde_json::Value::as_str);
    if target != Some("embedded") {
        panic!("querymt-ui.json target must be 'embedded', got {target:?}");
    }
    let acp_path = manifest
        .get("acpWebSocketPath")
        .and_then(serde_json::Value::as_str);
    if acp_path != Some("/acp/ws") {
        panic!("querymt-ui.json acpWebSocketPath must be '/acp/ws', got {acp_path:?}");
    }
}

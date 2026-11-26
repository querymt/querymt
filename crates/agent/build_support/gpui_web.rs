//! Prepares the web UI that the `dashboard` feature embeds and `qmtcode --dashboard`
//! serves: QueryMT Desktop (`crates/querymt-gpui`) built for `wasm32-unknown-unknown`,
//! or a prebuilt directory named by `QMT_UI_DIST`.
//!
//! Used by `build.rs` (which supplies the wasm-bindgen step) and compiled as the
//! `embedded_ui` test target. It only uses std so the tests need no extra dependencies.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The workspace profile the embedded wasm is built with (root `Cargo.toml`).
pub(crate) const WASM_PROFILE: &str = "release-gpui-web";
const WASM_TARGET: &str = "wasm32-unknown-unknown";
/// The JS/wasm file stem that `crates/querymt-gpui/web/index.html` imports.
pub(crate) const OUT_NAME: &str = "querymt_gpui";
const MANIFEST: &str = "querymt-ui.json";

#[derive(Debug)]
pub(crate) struct PreparedUi {
    pub(crate) embed_dir: PathBuf,
    pub(crate) source_log: String,
    pub(crate) watched_paths: Vec<PathBuf>,
}

/// Turns `<wasm>` into `<out_dir>/querymt_gpui.js` + `querymt_gpui_bg.wasm`.
pub(crate) type Bindgen<'a> = &'a dyn Fn(&Path, &Path) -> Result<(), String>;

/// Stages the UI into `<out_dir>/querymt-ui` and returns it with the paths to watch.
pub(crate) fn prepare_ui(
    out_dir: &Path,
    workspace_root: &Path,
    ui_dist: Option<&Path>,
    version: &str,
    bindgen: Bindgen<'_>,
) -> Result<PreparedUi, String> {
    let destination = out_dir.join("querymt-ui");
    let staging = out_dir.join(".querymt-ui-staging");
    remove_path_if_exists(&staging)?;
    fs::create_dir_all(&staging)
        .map_err(|err| format!("failed to create {}: {err}", staging.display()))?;

    let result = (|| {
        let (source_log, watched_paths) = match ui_dist {
            Some(dist) => {
                stage_prebuilt(dist, &staging)?;
                (
                    format!(
                        "querymt-ui: embedding the prebuilt UI from {} (QMT_UI_DIST)",
                        dist.display()
                    ),
                    vec![dist.to_path_buf()],
                )
            }
            None => {
                let optimized = build_gpui_web(workspace_root, &staging, bindgen)?;
                (
                    format!(
                        "querymt-ui: embedding QueryMT Desktop web (crates/querymt-gpui, profile {WASM_PROFILE}{})",
                        if optimized {
                            ", wasm-opt"
                        } else {
                            "; wasm-opt not on PATH, wasm left unoptimized"
                        }
                    ),
                    gpui_watched_paths(workspace_root),
                )
            }
        };
        write_manifest_if_missing(&staging, version)?;
        remove_path_if_exists(&destination)?;
        fs::rename(&staging, &destination).map_err(|err| {
            format!(
                "failed to move {} to {}: {err}",
                staging.display(),
                destination.display()
            )
        })?;
        Ok(PreparedUi {
            embed_dir: destination,
            source_log,
            watched_paths,
        })
    })();

    if result.is_err() {
        let _ = remove_path_if_exists(&staging);
    }
    result
}

/// Copies a prebuilt UI directory (for example the CI test fixture or the release
/// job's `desktop-web` output). It must contain `index.html`.
fn stage_prebuilt(dist: &Path, staging: &Path) -> Result<(), String> {
    let metadata = fs::metadata(dist).map_err(|err| {
        format!(
            "QMT_UI_DIST must be a directory containing index.html ({}): {err}",
            dist.display()
        )
    })?;
    if !metadata.is_dir() {
        return Err(format!(
            "QMT_UI_DIST must be a directory containing index.html: {}",
            dist.display()
        ));
    }
    if !dist.join("index.html").is_file() {
        return Err(format!(
            "QMT_UI_DIST has no index.html: {}",
            dist.join("index.html").display()
        ));
    }
    copy_directory(dist, staging)
}

/// Builds `querymt-gpui` for wasm32 with a nested cargo, then runs wasm-bindgen and,
/// when it is on PATH, wasm-opt. Returns whether wasm-opt ran.
fn build_gpui_web(
    workspace_root: &Path,
    staging: &Path,
    bindgen: Bindgen<'_>,
) -> Result<bool, String> {
    let gpui = workspace_root.join("crates/querymt-gpui");
    if !gpui.join("Cargo.toml").is_file() {
        return Err(format!(
            "the dashboard embeds QueryMT Desktop from {}, which is missing; build inside the \
             QueryMT workspace or set QMT_UI_DIST to a prebuilt UI directory",
            gpui.display()
        ));
    }

    // A separate target directory: the outer cargo holds the lock on the main one,
    // so building in it from a build script would deadlock.
    let target_dir = workspace_target_dir(workspace_root).join("querymt-gpui-web");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(&cargo);
    command
        .current_dir(workspace_root)
        .arg("build")
        .arg("--manifest-path")
        .arg(workspace_root.join("Cargo.toml"))
        .args(["-p", "querymt-gpui", "--bin", "querymt-gpui"])
        .args(["--target", WASM_TARGET, "--profile", WASM_PROFILE])
        .arg("--target-dir")
        .arg(&target_dir)
        // gpui-pre-web pulls `wasm_thread`, which uses a nightly-only feature gate
        // (the app still runs single-threaded); see crates/querymt-gpui/web/build.sh.
        .env("RUSTC_BOOTSTRAP", "1");
    for (name, _) in std::env::vars_os() {
        if clear_for_inner_cargo(&name) {
            command.env_remove(&name);
        }
    }
    let status = command.status().map_err(|err| {
        format!(
            "failed to run {} to build QueryMT Desktop web: {err}",
            Path::new(&cargo).display()
        )
    })?;
    if !status.success() {
        return Err(format!(
            "building QueryMT Desktop web failed ({status}); needs the {WASM_TARGET} target \
             (`rustup target add {WASM_TARGET}`, listed in rust-toolchain.toml). Set QMT_UI_DIST \
             to a prebuilt UI directory to skip this build"
        ));
    }

    let wasm = target_dir
        .join(WASM_TARGET)
        .join(WASM_PROFILE)
        .join("querymt-gpui.wasm");
    bindgen(&wasm, staging)?;
    verify_bindgen_output(staging)?;

    let optimized = run_wasm_opt(&staging.join(format!("{OUT_NAME}_bg.wasm")))?;
    fs::copy(gpui.join("web/index.html"), staging.join("index.html"))
        .map_err(|err| format!("failed to copy crates/querymt-gpui/web/index.html: {err}"))?;
    Ok(optimized)
}

/// Checks that wasm-bindgen produced a web loader that can find its wasm on its own,
/// like the CLI's `--target web` output: `init()` with no argument fetches
/// `<name>_bg.wasm` next to the JS. The library's default omits that fallback, which
/// leaves a blank page.
pub(crate) fn verify_bindgen_output(directory: &Path) -> Result<(), String> {
    let wasm = directory.join(format!("{OUT_NAME}_bg.wasm"));
    if !wasm.is_file() {
        return Err(format!("wasm-bindgen did not write {}", wasm.display()));
    }
    let js_path = directory.join(format!("{OUT_NAME}.js"));
    let js = fs::read_to_string(&js_path).map_err(|err| {
        format!(
            "wasm-bindgen did not write a readable {}: {err}",
            js_path.display()
        )
    })?;
    let default_path = format!("new URL('{OUT_NAME}_bg.wasm', import.meta.url)");
    if !js.contains(&default_path) {
        return Err(format!(
            "{} has no default wasm path (`{default_path}`); wasm-bindgen must run with \
             omit_default_module_path(false) and the web target",
            js_path.display()
        ));
    }
    Ok(())
}

/// Environment that would make the nested wasm build follow the outer build instead
/// of its own command line: the outer target, flags, target dir and profile overrides,
/// clippy's workspace wrapper, and the per-build-script variables. Registry, network,
/// jobserver and cargo home settings stay.
pub(crate) fn clear_for_inner_cargo(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    const KEEP: &[&str] = &[
        "CARGO_HOME",
        "CARGO_MAKEFLAGS",
        "CARGO_NET_OFFLINE",
        "CARGO_INCREMENTAL",
        "CARGO_BUILD_JOBS",
    ];
    const KEEP_PREFIXES: &[&str] = &[
        "CARGO_HTTP_",
        "CARGO_NET_",
        "CARGO_REGISTRIES_",
        "CARGO_REGISTRY_",
        "CARGO_TERM_",
    ];
    const CLEAR: &[&str] = &[
        "RUSTFLAGS",
        "RUSTDOCFLAGS",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTC_LINKER",
        "TARGET",
        "HOST",
        "OUT_DIR",
        "OPT_LEVEL",
        "PROFILE",
        "DEBUG",
        "NUM_JOBS",
    ];
    if CLEAR.contains(&name) || name.starts_with("__CARGO_") {
        return true;
    }
    name.starts_with("CARGO_")
        && !KEEP.contains(&name)
        && !KEEP_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

fn workspace_target_dir(workspace_root: &Path) -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        Some(dir) => workspace_root.join(dir),
        None => workspace_root.join("target"),
    }
}

/// Same flags as crates/querymt-gpui/web/build.sh. LTO can retain atomic
/// instructions even though the app uses unshared memory.
fn run_wasm_opt(wasm: &Path) -> Result<bool, String> {
    let available = Command::new("wasm-opt")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !available {
        return Ok(false);
    }
    let status = Command::new("wasm-opt")
        .args(["-Oz", "--strip-debug", "--strip-producers"])
        .args([
            "--enable-threads",
            "--enable-bulk-memory",
            "--enable-nontrapping-float-to-int",
            "--enable-sign-ext",
            "--enable-mutable-globals",
            "--enable-reference-types",
        ])
        .arg(wasm)
        .arg("-o")
        .arg(wasm)
        .status()
        .map_err(|err| format!("failed to run wasm-opt: {err}"))?;
    if !status.success() {
        return Err(format!("wasm-opt failed on {} ({status})", wasm.display()));
    }
    Ok(true)
}

/// What the nested build reads: the app and the workspace crates it depends on.
/// Cargo scans watched directories recursively.
fn gpui_watched_paths(workspace_root: &Path) -> Vec<PathBuf> {
    [
        "Cargo.toml",
        "Cargo.lock",
        "crates/querymt-gpui/Cargo.toml",
        "crates/querymt-gpui/build.rs",
        "crates/querymt-gpui/src",
        "crates/querymt-gpui/assets",
        "crates/querymt-gpui/web/index.html",
        "crates/agent-session-core",
        "crates/agent-protocol-types",
        "crates/querymt",
    ]
    .iter()
    .map(|path| workspace_root.join(path))
    .collect()
}

/// `/querymt-ui.json` tells clients which UI the server embeds and where its
/// same-origin ACP endpoint is.
fn write_manifest_if_missing(staging: &Path, version: &str) -> Result<(), String> {
    let path = staging.join(MANIFEST);
    if path.exists() {
        return Ok(());
    }
    let version = version.replace(['\\', '"'], "");
    fs::write(
        &path,
        format!(
            "{{\"target\":\"embedded\",\"acpWebSocketPath\":\"/acp/ws\",\"app\":\"querymt-gpui\",\"revision\":\"{version}\"}}\n"
        ),
    )
    .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn copy_directory(source: &Path, destination: &Path) -> Result<(), String> {
    let entries = fs::read_dir(source)
        .map_err(|err| format!("failed to read {}: {err}", source.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("failed to read {}: {err}", source.display()))?;
        let path = entry.path();
        let target = destination.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|err| format!("failed to inspect {}: {err}", path.display()))?;
        if kind.is_dir() {
            fs::create_dir_all(&target)
                .map_err(|err| format!("failed to create {}: {err}", target.display()))?;
            copy_directory(&path, &target)?;
        } else if kind.is_file() {
            fs::copy(&path, &target).map_err(|err| {
                format!(
                    "failed to copy {} to {}: {err}",
                    path.display(),
                    target.display()
                )
            })?;
        } else {
            return Err(format!(
                "QMT_UI_DIST may only contain files and directories: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn remove_path_if_exists(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(format!("failed to inspect {}: {err}", path.display())),
    }
    .map_err(|err| format!("failed to remove {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn no_bindgen(_: &Path, _: &Path) -> Result<(), String> {
        panic!("a prebuilt UI must not trigger the wasm build")
    }

    #[test]
    fn prebuilt_ui_is_embedded_with_a_manifest() {
        let dist = tempfile::tempdir().unwrap();
        fs::write(dist.path().join("index.html"), "<title>QueryMT</title>").unwrap();
        fs::create_dir(dist.path().join("assets")).unwrap();
        fs::write(dist.path().join("assets/app.wasm"), b"\0asm").unwrap();
        let out = tempfile::tempdir().unwrap();

        let prepared = prepare_ui(
            out.path(),
            Path::new("/nonexistent-workspace"),
            Some(dist.path()),
            "0.6.0",
            &no_bindgen,
        )
        .unwrap();

        assert!(prepared.embed_dir.join("assets/app.wasm").is_file());
        let manifest = fs::read_to_string(prepared.embed_dir.join(MANIFEST)).unwrap();
        assert!(manifest.contains("\"acpWebSocketPath\":\"/acp/ws\""));
        assert!(manifest.contains("\"revision\":\"0.6.0\""));
        assert_eq!(prepared.watched_paths, vec![dist.path().to_path_buf()]);
        assert!(prepared.source_log.contains("QMT_UI_DIST"));
    }

    #[test]
    fn prebuilt_ui_without_index_is_rejected() {
        let dist = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let err = prepare_ui(
            out.path(),
            Path::new("/nonexistent-workspace"),
            Some(dist.path()),
            "0.6.0",
            &no_bindgen,
        )
        .unwrap_err();
        assert!(err.contains("no index.html"), "{err}");
        assert!(!out.path().join("querymt-ui").exists());
    }

    fn write_bindgen_output(directory: &Path, js: &str) {
        fs::write(directory.join(format!("{OUT_NAME}.js")), js).unwrap();
        fs::write(directory.join(format!("{OUT_NAME}_bg.wasm")), b"\0asm").unwrap();
    }

    #[test]
    fn web_loader_with_a_default_wasm_path_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        write_bindgen_output(
            dir.path(),
            "if (module_or_path === undefined) {\n    module_or_path = new URL('querymt_gpui_bg.wasm', import.meta.url);\n}",
        );
        verify_bindgen_output(dir.path()).unwrap();
    }

    #[test]
    fn web_loader_without_a_default_wasm_path_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write_bindgen_output(
            dir.path(),
            "async function __wbg_init_once(module_or_path) { const imports = {}; }",
        );
        let err = verify_bindgen_output(dir.path()).unwrap_err();
        assert!(err.contains("omit_default_module_path(false)"), "{err}");
    }

    #[test]
    fn missing_wasm_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(format!("{OUT_NAME}.js")), "").unwrap();
        let err = verify_bindgen_output(dir.path()).unwrap_err();
        assert!(err.contains("did not write"), "{err}");
    }

    #[test]
    fn inner_wasm_build_does_not_inherit_the_outer_build() {
        for cleared in [
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_TARGET",
            "CARGO_PROFILE_RELEASE_LTO",
            "RUSTC_WORKSPACE_WRAPPER",
            "RUSTFLAGS",
            "__CARGO_DEFAULT_LIB_METADATA",
        ] {
            assert!(clear_for_inner_cargo(&OsString::from(cleared)), "{cleared}");
        }
        for kept in [
            "CARGO_HOME",
            "CARGO_NET_OFFLINE",
            "CARGO_MAKEFLAGS",
            "CARGO_REGISTRIES_CRATES_IO_PROTOCOL",
            "RUSTC_WRAPPER",
            "QMT_VERSION",
        ] {
            assert!(!clear_for_inner_cargo(&OsString::from(kept)), "{kept}");
        }
    }
}

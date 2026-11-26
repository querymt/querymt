#[cfg(feature = "dashboard")]
#[path = "build_support/gpui_web.rs"]
mod gpui_web;

fn main() {
    #[cfg(feature = "dashboard")]
    prepare_dashboard();
}

/// Embeds QueryMT Desktop web (crates/querymt-gpui built for wasm32) for
/// `qmtcode --dashboard`, or the prebuilt directory named by QMT_UI_DIST.
#[cfg(feature = "dashboard")]
fn prepare_dashboard() {
    use std::path::{Path, PathBuf};

    println!("cargo:rerun-if-env-changed=QMT_UI_DIST");
    println!("cargo:rerun-if-env-changed=QMT_VERSION");

    let out_dir = PathBuf::from(
        std::env::var_os("OUT_DIR").unwrap_or_else(|| panic!("Cargo did not set OUT_DIR")),
    );
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR")
            .unwrap_or_else(|| panic!("Cargo did not set CARGO_MANIFEST_DIR")),
    );
    let workspace_root = manifest_dir
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| manifest_dir.join("../.."));
    let ui_dist = std::env::var_os("QMT_UI_DIST")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let version = std::env::var("QMT_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").unwrap_or_default());
    let version = version.trim().trim_start_matches('v');

    let bindgen = |wasm: &Path, out: &Path| -> Result<(), String> {
        let mut bindgen = wasm_bindgen_cli_support::Bindgen::new();
        // Same output as `wasm-bindgen --target web --no-typescript`. The library
        // defaults to omitting the default wasm path (`new URL('<name>_bg.wasm',
        // import.meta.url)`), the CLI does not; without it `init()` cannot find the
        // wasm. gpui_web::prepare_ui checks the result.
        bindgen
            .input_path(wasm)
            .out_name(gpui_web::OUT_NAME)
            .typescript(false)
            .omit_default_module_path(false);
        bindgen
            .web(true)
            .map_err(|err| format!("wasm-bindgen: {err:#}"))?;
        bindgen
            .generate(out)
            .map_err(|err| format!("wasm-bindgen on {}: {err:#}", wasm.display()))
    };

    let prepared = gpui_web::prepare_ui(
        &out_dir,
        &workspace_root,
        ui_dist.as_deref(),
        version,
        &bindgen,
    )
    .unwrap_or_else(|err| panic!("failed to prepare the embedded dashboard UI: {err}"));

    for path in prepared.watched_paths {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!(
        "cargo:rustc-env=QMT_UI_EMBED_DIR={}",
        prepared.embed_dir.display()
    );
    println!("cargo:warning={}", prepared.source_log);
}

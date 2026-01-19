#[cfg(feature = "dashboard")]
use std::path::Path;
#[cfg(feature = "dashboard")]
use std::process::Command;

fn main() {
    // Only build UI when dashboard feature is enabled
    #[cfg(feature = "dashboard")]
    build_ui();
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

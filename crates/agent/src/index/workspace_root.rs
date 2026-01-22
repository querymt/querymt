use std::path::{Path, PathBuf};

const ROOT_MARKERS: [&str; 4] = [".git", "Cargo.toml", "package.json", "pyproject.toml"];

pub fn resolve_workspace_root(cwd: &Path) -> PathBuf {
    let mut current = normalize_cwd(cwd);

    loop {
        if has_root_marker(&current) {
            return current;
        }

        if !current.pop() {
            return normalize_cwd(cwd);
        }
    }
}

pub fn normalize_cwd(cwd: &Path) -> PathBuf {
    if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|base| base.join(cwd))
            .unwrap_or_else(|_| cwd.to_path_buf())
    }
}

fn has_root_marker(dir: &Path) -> bool {
    ROOT_MARKERS.iter().any(|marker| dir.join(marker).exists())
}

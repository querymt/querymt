//! Computes `QMT_CODE_INDEX_FINGERPRINT`, part of the persistent code cache's epoch.
//!
//! Stored symbols and function entries depend on the extractor sources in this crate and on
//! the parser crates they use (tree-sitter grammars, the `similarity-*` fork, oxc, syn). The
//! package version alone does not change when those do (nightlies, local builds, a grammar
//! bump), so this hash of both is folded into the epoch and any change discards old caches.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Sources whose behaviour determines stored results.
const SOURCE_PATHS: &[&str] = &[
    "src/symbol_index",
    "src/function_index",
    "src/outline_index/common.rs",
    "src/code_cache/values.rs",
    "src/code_cache/queries.rs",
];

/// Lockfile packages whose versions determine stored results.
const PARSER_PACKAGE_PREFIXES: &[&str] = &[
    "tree-sitter",
    "similarity-",
    "oxc",
    "syn",
    "ajika",
    "wincode",
    "rapidhash",
];

/// 64-bit FNV-1a: deterministic across toolchains and platforms, unlike `DefaultHasher`.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
        // Separator, so ("ab", "c") and ("a", "bc") differ.
        self.0 ^= 0xff;
        self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
    }
}

fn collect_files(path: &Path, files: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            collect_files(&entry.path(), files);
        }
    } else if path.extension().is_some_and(|ext| ext == "rs") {
        files.push(path.to_path_buf());
    }
}

fn find_lockfile(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|dir| dir.join("Cargo.lock"))
        .find(|lock| lock.is_file())
}

fn hash_parser_packages(lockfile: &str, hash: &mut Fnv) {
    for package in lockfile.split("[[package]]").skip(1) {
        let field = |key: &str| {
            package.lines().find_map(|line| {
                line.strip_prefix(key)
                    .and_then(|rest| rest.trim_start().strip_prefix('='))
                    .map(|value| value.trim().trim_matches('"').to_owned())
            })
        };
        let Some(name) = field("name") else {
            continue;
        };
        if !PARSER_PACKAGE_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
        {
            continue;
        }
        hash.write(name.as_bytes());
        hash.write(field("version").unwrap_or_default().as_bytes());
        hash.write(field("source").unwrap_or_default().as_bytes());
    }
}

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let mut hash = Fnv::new();

    let mut files = Vec::new();
    for source in SOURCE_PATHS {
        let path = manifest.join(source);
        println!("cargo:rerun-if-changed={}", path.display());
        collect_files(&path, &mut files);
    }
    files.sort();
    for file in files {
        let relative = file.strip_prefix(&manifest).unwrap_or(&file);
        hash.write(relative.to_string_lossy().replace('\\', "/").as_bytes());
        // Normalise line endings so a CRLF checkout fingerprints like an LF one.
        let text = fs::read_to_string(&file)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        hash.write(text.as_bytes());
    }

    // Outside a workspace (for example a registry build without a lockfile) only the sources
    // and the package version identify the extractors.
    if let Some(lockfile) = find_lockfile(&manifest) {
        println!("cargo:rerun-if-changed={}", lockfile.display());
        hash_parser_packages(
            &fs::read_to_string(&lockfile).unwrap_or_default(),
            &mut hash,
        );
    }

    println!("cargo:rustc-env=QMT_CODE_INDEX_FINGERPRINT={:016x}", hash.0);
}

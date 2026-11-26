use std::{
    env, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use sha2::{Digest, Sha256};

// The same function the app uses to read its channel back (src/update).
#[path = "src/update/channel.rs"]
mod channel;

// IBM Plex v6.4.0: these files match the previously embedded font binaries.
const REVISION: &str = "383c681f015ed2626e919ec4e3cca16ccc204e9d";
const DOWNLOAD_BASE: &str = "https://raw.githubusercontent.com/IBM/plex";
const MAX_FONT_SIZE: u64 = 1024 * 1024;
const FONTS: &[(&str, &str, &str)] = &[
    (
        "Sans",
        "IBMPlexSans-Regular.ttf",
        "975dcda37d80f038dcd143c22e33ca2d97a0cc5a929aace1c749153b0fe1afa5",
    ),
    (
        "Sans",
        "IBMPlexSans-Medium.ttf",
        "331c8639d7598b2cde62a911a71db195e30cb655cd6bdf2e324a7e984955f907",
    ),
    (
        "Sans",
        "IBMPlexSans-SemiBold.ttf",
        "a20caf8286023a6a7a85e40b1d2a4ae9fc3e3b1f9eda8f4c542dd4986af67bb1",
    ),
    (
        "Sans",
        "IBMPlexSans-Bold.ttf",
        "9e6c74a889a700d707613d24548fe4ffa6bc59559a0689d2cf9e133bdcdafb2f",
    ),
    (
        "Mono",
        "IBMPlexMono-Regular.ttf",
        "fe11304a5fe956d5744e9b6a246cc83d90425245e75a62230044966ca96a7f50",
    ),
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for variable in [
        "QMT_FONT_CACHE",
        "CARGO_NET_OFFLINE",
        "XDG_CACHE_HOME",
        "HOME",
        "LOCALAPPDATA",
        "USERPROFILE",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    prepare_fonts().unwrap_or_else(|err| panic!("failed to prepare embedded fonts: {err}"));
    set_build_version();
}

/// The version the app reports (`util::APP_VERSION`): release and nightly CI pass
/// the published version (`v0.6.0`, `0.6.0-nightly.<timestamp>`) as `QMT_VERSION`,
/// like the qmt binaries; local builds fall back to the crate version.
fn set_build_version() {
    println!("cargo:rerun-if-env-changed=QMT_VERSION");
    let version = env::var("QMT_VERSION")
        .ok()
        .filter(|version| !version.trim().is_empty())
        .unwrap_or_else(|| env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into()));
    let version = version.trim();
    let normalized = version.strip_prefix('v').unwrap_or(version);
    println!("cargo:rustc-env=QMT_BUILD_VERSION={normalized}");
    set_update_channel();
}

/// The update channel of this build (`dev`, `nightly` or `stable`). Only a
/// build with a release version (`QMT_VERSION`, set by CI) can update itself;
/// `dev` builds make no update requests.
fn set_update_channel() {
    println!("cargo:rerun-if-changed=src/update/channel.rs");
    let version = env::var("QMT_VERSION").ok();
    println!(
        "cargo:rustc-env=QMT_UPDATE_CHANNEL={}",
        channel::name_for_version(version.as_deref())
    );
}

fn prepare_fonts() -> Result<(), String> {
    let cache = cache_dir()?.join(REVISION);
    let out_dir = env::var_os("OUT_DIR").ok_or("Cargo did not set OUT_DIR")?;
    let destination = PathBuf::from(out_dir).join("fonts");
    for directory in [&cache, &destination] {
        fs::create_dir_all(directory)
            .map_err(|err| format!("failed to create {}: {err}", directory.display()))?;
    }
    let offline = env::var("CARGO_NET_OFFLINE")
        .is_ok_and(|value| value.eq_ignore_ascii_case("true") || value == "1");

    for &(family, name, digest) in FONTS {
        let cached = cache.join(name);
        let contents = match read_cached(&cached, digest)? {
            Some(contents) => contents,
            None if offline => {
                return Err(format!(
                    "font {name} is missing or corrupt in {} and CARGO_NET_OFFLINE is set; \
                     populate QMT_FONT_CACHE with an online build first",
                    cache.display(),
                ));
            }
            None => {
                let url = format!(
                    "{DOWNLOAD_BASE}/{REVISION}/IBM-Plex-{family}/fonts/complete/ttf/{name}"
                );
                eprintln!("Downloading font {name}");
                let contents = download(&url)?;
                verify_digest(&contents, digest).map_err(|err| format!("font {name}: {err}"))?;
                // Publish only verified, complete files. Separate temporary names
                // also allow native and WASM builds to share this cache safely.
                let mut temporary = tempfile::NamedTempFile::new_in(&cache)
                    .map_err(|err| format!("failed to stage font {name}: {err}"))?;
                temporary
                    .write_all(&contents)
                    .map_err(|err| format!("failed to write font {name}: {err}"))?;
                temporary
                    .persist(&cached)
                    .map_err(|err| format!("failed to cache font {}: {err}", cached.display()))?;
                contents
            }
        };
        fs::write(destination.join(name), contents)
            .map_err(|err| format!("failed to stage embedded font {name}: {err}"))?;
        println!("cargo:rerun-if-changed={}", cached.display());
    }
    Ok(())
}

fn cache_dir() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("QMT_FONT_CACHE") {
        if path.is_empty() {
            return Err("QMT_FONT_CACHE must not be empty".into());
        }
        return Ok(PathBuf::from(path));
    }
    dirs::cache_dir()
        .map(|path| path.join("querymt-gpui/fonts"))
        .ok_or_else(|| "could not locate the OS cache directory; set QMT_FONT_CACHE".into())
}

fn read_cached(path: &Path, digest: &str) -> Result<Option<Vec<u8>>, String> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(format!(
                "failed to read cached font {}: {err}",
                path.display()
            ));
        }
    };
    let mut contents = Vec::new();
    file.take(MAX_FONT_SIZE + 1)
        .read_to_end(&mut contents)
        .map_err(|err| format!("failed to read cached font {}: {err}", path.display()))?;
    Ok(verify_digest(&contents, digest).is_ok().then_some(contents))
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    let config = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(5)
        .timeout_global(Some(Duration::from_secs(120)))
        .timeout_connect(Some(Duration::from_secs(15)))
        .user_agent("querymt-gpui-build")
        .build();
    let mut response = ureq::Agent::new_with_config(config)
        .get(url)
        .call()
        .map_err(|err| format!("failed to download {url}: {err}"))?;
    let contents = response
        .body_mut()
        .with_config()
        .limit(MAX_FONT_SIZE + 1)
        .read_to_vec()
        .map_err(|err| format!("failed to read {url}: {err}"))?;
    if contents.len() as u64 > MAX_FONT_SIZE {
        return Err(format!(
            "font download from {url} exceeds {MAX_FONT_SIZE} bytes"
        ));
    }
    Ok(contents)
}

fn verify_digest(contents: &[u8], expected: &str) -> Result<(), String> {
    let actual = format!("{:x}", Sha256::digest(contents));
    if actual != expected {
        return Err(format!(
            "SHA-256 mismatch: expected {expected}, got {actual}"
        ));
    }
    Ok(())
}

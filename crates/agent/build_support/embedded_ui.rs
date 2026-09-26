use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Cursor, Read};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/querymt/querymt-desktop/releases/latest";
const COMMIT_API_BASE: &str = "https://api.github.com/repos/querymt/querymt-desktop/commits";
const RELEASE_DOWNLOAD_BASE: &str = "https://github.com/querymt/querymt-desktop/releases/download";
const SOURCE_ARCHIVE_BASE: &str = "https://codeload.github.com/querymt/querymt-desktop/tar.gz";
const MAX_ARCHIVE_SIZE: u64 = 64 * 1024 * 1024;
const MAX_EXTRACTED_SIZE: u64 = 256 * 1024 * 1024;
const MAX_DECOMPRESSED_SIZE: u64 = 320 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: u64 = 20_000;
const MAX_ARCHIVE_PATH_LENGTH: usize = 4_096;
const MAX_SOURCE_ARCHIVE_SIZE: u64 = 128 * 1024 * 1024;
const MAX_SOURCE_EXTRACTED_SIZE: u64 = 1024 * 1024 * 1024;
const MAX_SOURCE_DECOMPRESSED_SIZE: u64 = 1152 * 1024 * 1024;
const MAX_SOURCE_ARCHIVE_ENTRIES: u64 = 100_000;
const MAX_SOURCE_ARCHIVE_PATH_LENGTH: usize = 8_192;
const MAX_METADATA_SIZE: u64 = 64 * 1024;
const MAX_COMMIT_RESPONSE_SIZE: u64 = 1024 * 1024;
const USER_AGENT: &str = "querymt-build-script";

#[derive(Debug)]
pub(crate) struct PreparedUi {
    pub(crate) embed_dir: PathBuf,
    pub(crate) source_log: String,
    pub(crate) watched_paths: Vec<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReleaseSelector {
    Latest,
    Release(String),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RevisionSpec {
    Source(String),
    Release {
        selector: ReleaseSelector,
        archive_sha256: [u8; 32],
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ReleaseAssets {
    pub(crate) revision: String,
    pub(crate) archive_url: String,
    pub(crate) checksum_url: String,
}

#[derive(Clone, Copy)]
struct ArchiveLimits {
    compressed: u64,
    decompressed: u64,
    extracted: u64,
    entries: u64,
    path_length: usize,
}

const ARCHIVE_LIMITS: ArchiveLimits = ArchiveLimits {
    compressed: MAX_ARCHIVE_SIZE,
    decompressed: MAX_DECOMPRESSED_SIZE,
    extracted: MAX_EXTRACTED_SIZE,
    entries: MAX_ARCHIVE_ENTRIES,
    path_length: MAX_ARCHIVE_PATH_LENGTH,
};

const SOURCE_ARCHIVE_LIMITS: ArchiveLimits = ArchiveLimits {
    compressed: MAX_SOURCE_ARCHIVE_SIZE,
    decompressed: MAX_SOURCE_DECOMPRESSED_SIZE,
    extracted: MAX_SOURCE_EXTRACTED_SIZE,
    entries: MAX_SOURCE_ARCHIVE_ENTRIES,
    path_length: MAX_SOURCE_ARCHIVE_PATH_LENGTH,
};

pub(crate) fn select_revision(revision: &str) -> Result<RevisionSpec, String> {
    validate_revision_text(revision)?;

    if let Some(digest) = revision.strip_prefix("sha256:") {
        return Ok(RevisionSpec::Release {
            selector: ReleaseSelector::Latest,
            archive_sha256: parse_digest(digest)?,
        });
    }

    if let Some((release, digest)) = revision.rsplit_once("@sha256:") {
        validate_release_name(release)?;
        return Ok(RevisionSpec::Release {
            selector: if release == "latest" {
                ReleaseSelector::Latest
            } else {
                ReleaseSelector::Release(release.to_string())
            },
            archive_sha256: parse_digest(digest)?,
        });
    }

    Ok(RevisionSpec::Source(revision.to_string()))
}

fn validate_revision_text(revision: &str) -> Result<(), String> {
    if revision.is_empty() {
        return Err("QMT_UI_REVISION must not be empty".to_string());
    }
    if revision.chars().any(char::is_control) {
        return Err("QMT_UI_REVISION must not contain control characters".to_string());
    }
    Ok(())
}

fn validate_release_name(revision: &str) -> Result<(), String> {
    if revision.is_empty() {
        return Err("embedded UI release revision must not be empty".to_string());
    }
    if revision.chars().any(char::is_control) {
        return Err("embedded UI release revision must not contain control characters".to_string());
    }
    Ok(())
}

fn parse_digest(digest: &str) -> Result<[u8; 32], String> {
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(
            "QMT_UI_REVISION SHA-256 must be exactly 64 hexadecimal characters".to_string(),
        );
    }
    let mut parsed = [0_u8; 32];
    hex::decode_to_slice(digest, &mut parsed)
        .map_err(|err| format!("QMT_UI_REVISION contains an invalid SHA-256: {err}"))?;
    Ok(parsed)
}

pub(crate) fn release_assets(revision: &str) -> Result<ReleaseAssets, String> {
    validate_release_name(revision)?;

    let revision_segment = percent_encode_segment(revision);
    let archive_name = format!("querymt-embedded-ui-{revision}.tar.gz");
    let archive_segment = percent_encode_segment(&archive_name);
    let archive_url = format!("{RELEASE_DOWNLOAD_BASE}/{revision_segment}/{archive_segment}");

    Ok(ReleaseAssets {
        revision: revision.to_string(),
        checksum_url: format!("{archive_url}.sha256"),
        archive_url,
    })
}

pub(crate) fn source_archive_url(revision: &str) -> Result<String, String> {
    validate_revision_text(revision)?;
    Ok(format!(
        "{SOURCE_ARCHIVE_BASE}/{}",
        percent_encode_segment(revision)
    ))
}

fn selected_source_revision<'a>(
    revision: Option<&'a str>,
    pinned_revision: &'a str,
) -> (&'a str, bool) {
    (revision.unwrap_or(pinned_revision), revision.is_none())
}

fn source_revision_log(requested: &str, resolved: &str, is_default: bool) -> String {
    if is_default {
        format!("querymt-ui: source = pinned source revision {resolved}")
    } else {
        format!("querymt-ui: source = source revision {requested} (resolved {resolved})")
    }
}

fn source_mode_watched_paths() -> Vec<PathBuf> {
    vec![
        PathBuf::from("build.rs"),
        PathBuf::from("build_support/embedded_ui.rs"),
    ]
}

pub(crate) fn prepare_ui(
    out_dir: &Path,
    ui_dist: Option<&OsStr>,
    revision: Option<&str>,
    pinned_revision: &str,
) -> Result<PreparedUi, String> {
    let destination = out_dir.join("querymt-ui");
    let staging = out_dir.join(".querymt-ui-staging");
    let backup = out_dir.join(".querymt-ui-backup");
    let source_root = out_dir.join(".querymt-ui-source");

    fs::create_dir_all(out_dir)
        .map_err(|err| format!("failed to create OUT_DIR {}: {err}", out_dir.display()))?;
    if let Some(source) = ui_dist {
        reject_directory_out_dir_overlap(Path::new(source), out_dir)?;
    }

    recover_stale_backup(&destination, &backup)?;
    remove_path_if_exists(&staging)?;
    remove_path_if_exists(&backup)?;
    remove_path_if_exists(&source_root)?;
    fs::create_dir(&staging).map_err(|err| {
        format!(
            "failed to create fresh embedded UI staging directory {}: {err}",
            staging.display()
        )
    })?;

    let result = (|| {
        let (source_log, watched_paths, expected_revision) = match ui_dist {
            Some(path) => stage_local_source(Path::new(path), &staging)?,
            None => {
                let (revision, is_default) = selected_source_revision(revision, pinned_revision);
                stage_remote_source(revision, is_default, &source_root, &staging)?
            }
        };

        validate_manifest(&staging, expected_revision.as_deref())?;
        swap_staging_directory(&staging, &destination, &backup)?;

        Ok(PreparedUi {
            embed_dir: destination,
            source_log,
            watched_paths,
        })
    })();

    if result.is_err() {
        let _ = remove_path_if_exists(&staging);
        let _ = remove_path_if_exists(&source_root);
    }
    result
}

fn reject_directory_out_dir_overlap(source: &Path, out_dir: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(()),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(());
    }

    let source = fs::canonicalize(source).map_err(|err| {
        format!(
            "failed to canonicalize QMT_UI_DIST directory {}: {err}",
            source.display()
        )
    })?;
    let out_dir = fs::canonicalize(out_dir).map_err(|err| {
        format!(
            "failed to canonicalize OUT_DIR {}: {err}",
            out_dir.display()
        )
    })?;
    if source.starts_with(&out_dir) || out_dir.starts_with(&source) {
        return Err(format!(
            "QMT_UI_DIST directory {} and OUT_DIR {} must not contain or overlap each other",
            source.display(),
            out_dir.display()
        ));
    }
    Ok(())
}

fn stage_local_source(
    source: &Path,
    staging: &Path,
) -> Result<(String, Vec<PathBuf>, Option<String>), String> {
    let metadata = fs::symlink_metadata(source).map_err(|err| {
        format!(
            "QMT_UI_DIST does not identify a readable directory or .tar.gz archive ({}): {err}",
            source.display()
        )
    })?;
    let file_type = metadata.file_type();

    if file_type.is_symlink() {
        return Err(format!(
            "QMT_UI_DIST must not be a symlink: {}",
            source.display()
        ));
    }

    if metadata.is_dir() {
        copy_directory(source, staging)?;
        return Ok((
            format!("querymt-ui: source = local directory {}", source.display()),
            vec![source.to_path_buf()],
            None,
        ));
    }

    if metadata.is_file() && has_tar_gz_suffix(source) {
        let archive = read_file_limited(source, MAX_ARCHIVE_SIZE, "embedded UI archive")?;
        let sidecar = sidecar_path(source);
        let checksum_status = match fs::symlink_metadata(&sidecar) {
            Ok(sidecar_metadata) => {
                if !sidecar_metadata.is_file() || sidecar_metadata.file_type().is_symlink() {
                    return Err(format!(
                        "embedded UI checksum sidecar must be a regular file: {}",
                        sidecar.display()
                    ));
                }
                let checksum =
                    read_file_limited(&sidecar, MAX_METADATA_SIZE, "embedded UI checksum sidecar")?;
                verify_sha256(&archive, &checksum)?;
                "adjacent SHA-256 sidecar verified"
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                "checksum not supplied; trusted explicit local override"
            }
            Err(err) => {
                return Err(format!(
                    "failed to inspect embedded UI checksum sidecar {}: {err}",
                    sidecar.display()
                ));
            }
        };

        extract_archive(&archive, staging)?;
        return Ok((
            format!(
                "querymt-ui: source = local archive {} ({checksum_status})",
                source.display()
            ),
            vec![source.to_path_buf(), sidecar],
            None,
        ));
    }

    Err(format!(
        "QMT_UI_DIST must be a directory or a regular file ending exactly .tar.gz: {}",
        source.display()
    ))
}

fn stage_remote_source(
    revision: &str,
    is_default: bool,
    source_root: &Path,
    staging: &Path,
) -> Result<(String, Vec<PathBuf>, Option<String>), String> {
    match select_revision(revision)? {
        RevisionSpec::Source(requested) => {
            stage_source_revision(&requested, is_default, source_root, staging)
        }
        RevisionSpec::Release {
            selector,
            archive_sha256,
        } => stage_release_archive(selector, archive_sha256, staging),
    }
}

fn stage_release_archive(
    selector: ReleaseSelector,
    archive_sha256: [u8; 32],
    staging: &Path,
) -> Result<(String, Vec<PathBuf>, Option<String>), String> {
    let agent = secure_http_agent();
    let (assets, source_commit, selection_log) = match selector {
        ReleaseSelector::Latest => {
            let latest_json = download_limited(&agent, LATEST_RELEASE_URL, MAX_METADATA_SIZE)
                .map_err(|err| unpublished_error("latest", &err))?;
            let tag =
                parse_latest_tag(&latest_json).map_err(|err| unpublished_error("latest", &err))?;
            let source_commit = resolve_source_commit(&agent, &tag, false)?;
            (
                release_assets(&tag)?,
                source_commit,
                format!("latest release tag {tag} (non-reproducible selection)"),
            )
        }
        ReleaseSelector::Release(release) => {
            let source_commit = resolve_source_commit(&agent, &release, true)?;
            (
                release_assets(&release)?,
                source_commit,
                format!("release {release}"),
            )
        }
    };

    let archive = download_limited(&agent, &assets.archive_url, MAX_ARCHIVE_SIZE)
        .map_err(|err| unpublished_error(&assets.revision, &err))?;
    verify_digest(&archive, &archive_sha256).map_err(|err| {
        format!("remote embedded UI archive content-pin verification failed: {err}")
    })?;
    extract_archive(&archive, staging)?;

    Ok((
        format!(
            "querymt-ui: source = remote archive {} ({selection_log}, source commit {source_commit}; user-supplied SHA-256 {} verified; content-pinned)",
            assets.archive_url,
            hex::encode(archive_sha256)
        ),
        Vec::new(),
        Some(source_commit),
    ))
}

fn stage_source_revision(
    requested: &str,
    is_default: bool,
    source_root: &Path,
    staging: &Path,
) -> Result<(String, Vec<PathBuf>, Option<String>), String> {
    let agent = secure_http_agent();
    let source_commit = resolve_source_commit(&agent, requested, true)?;
    let archive_url = source_archive_url(&source_commit)?;
    let archive = download_limited(&agent, &archive_url, MAX_SOURCE_ARCHIVE_SIZE)
        .map_err(|err| source_revision_error(requested, &err))?;

    fs::create_dir(source_root).map_err(|err| {
        format!(
            "failed to create embedded UI source directory {}: {err}",
            source_root.display()
        )
    })?;
    let source_directory = extract_source_archive(&archive, source_root)?;
    let build_result = build_source_artifact(
        &source_directory,
        staging,
        &source_commit,
        npm_program_for_target(cfg!(windows)),
    );
    let cleanup_result = remove_path_if_exists(source_root);
    match (build_result, cleanup_result) {
        (Err(build_error), _) => return Err(build_error),
        (Ok(()), Err(cleanup_error)) => return Err(cleanup_error),
        (Ok(()), Ok(())) => {}
    }

    Ok((
        source_revision_log(requested, &source_commit, is_default),
        source_mode_watched_paths(),
        Some(source_commit),
    ))
}

fn npm_program_for_target(is_windows: bool) -> &'static OsStr {
    OsStr::new(if is_windows { "npm.cmd" } else { "npm" })
}

fn build_source_artifact(
    source_directory: &Path,
    staging: &Path,
    source_commit: &str,
    npm_program: &OsStr,
) -> Result<(), String> {
    run_npm_command(source_directory, source_commit, npm_program, &["ci"])?;
    run_npm_command(
        source_directory,
        source_commit,
        npm_program,
        &["run", "test:embedded"],
    )?;
    run_npm_command(
        source_directory,
        source_commit,
        npm_program,
        &["run", "build:embedded"],
    )?;

    let artifact = source_directory.join("build-embedded");
    let metadata = fs::symlink_metadata(&artifact).map_err(|err| {
        format!(
            "npm run build:embedded did not produce {}: {err}",
            artifact.display()
        )
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!(
            "npm run build:embedded output must be a regular directory: {}",
            artifact.display()
        ));
    }
    copy_directory(&artifact, staging)
}

fn run_npm_command(
    source_directory: &Path,
    source_commit: &str,
    npm_program: &OsStr,
    arguments: &[&str],
) -> Result<(), String> {
    let path = std::env::var_os("PATH")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "PATH is required to launch Node.js/npm for the embedded querymt UI source build"
                .to_string()
        })?;
    let build_home = source_directory.join(".querymt-build-home");
    let npm_cache = source_directory.join(".querymt-npm-cache");
    for directory in [&build_home, &npm_cache] {
        fs::create_dir_all(directory).map_err(|err| {
            format!(
                "failed to create isolated npm build directory {}: {err}",
                directory.display()
            )
        })?;
    }

    eprintln!(
        "querymt-ui: running npm {} in {}",
        arguments.join(" "),
        source_directory.display()
    );
    let mut command = Command::new(npm_program);
    command
        .args(arguments)
        .current_dir(source_directory)
        .env_clear()
        .env("PATH", path)
        .env("HOME", &build_home)
        .env("USERPROFILE", &build_home)
        .env("XDG_CACHE_HOME", &npm_cache)
        .env("npm_config_cache", &npm_cache)
        .env("CI", "true")
        .env("QUERYMT_UI_REVISION", source_commit);

    for variable in [
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "NIX_SSL_CERT_FILE",
        "NODE_EXTRA_CA_CERTS",
    ] {
        if let Some(value) = std::env::var_os(variable) {
            command.env(variable, value);
        }
    }
    #[cfg(windows)]
    for variable in ["SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(variable) {
            command.env(variable, value);
        }
    }
    #[cfg(unix)]
    if let Some(value) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", value);
    }

    let status = command.status().map_err(|err| {
        if err.kind() == io::ErrorKind::NotFound {
            "npm is required to build the embedded querymt UI from source; install Node.js/npm or set QMT_UI_DIST to a prebuilt directory/archive"
                .to_string()
        } else {
            format!(
                "failed to run npm {} in {}: {err}",
                arguments.join(" "),
                source_directory.display()
            )
        }
    })?;
    if !status.success() {
        return Err(format!(
            "npm {} failed in {} with status {status}",
            arguments.join(" "),
            source_directory.display()
        ));
    }
    Ok(())
}

fn secure_http_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        // ureq applies https_only to every redirect target as well as the initial URL.
        .https_only(true)
        .max_redirects(5)
        .timeout_global(Some(Duration::from_secs(120)))
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(Duration::from_secs(60)))
        .user_agent(USER_AGENT)
        .accept("application/vnd.github+json")
        .build();
    ureq::Agent::new_with_config(config)
}

fn resolve_source_commit(
    agent: &ureq::Agent,
    release: &str,
    direct_sha_allowed: bool,
) -> Result<String, String> {
    if direct_sha_allowed && is_sha_revision(release) {
        return Ok(release.to_ascii_lowercase());
    }

    let url = format!("{COMMIT_API_BASE}/{}", percent_encode_segment(release));
    let response = download_limited(agent, &url, MAX_COMMIT_RESPONSE_SIZE).map_err(|err| {
        source_revision_error(release, &format!("source commit lookup failed: {err}"))
    })?;
    parse_commit_sha(&response).map_err(|err| {
        source_revision_error(release, &format!("source commit lookup failed: {err}"))
    })
}

fn download_limited(agent: &ureq::Agent, url: &str, limit: u64) -> Result<Vec<u8>, String> {
    validate_https_url(url)?;

    let mut response = agent
        .get(url)
        .call()
        .map_err(|err| format!("failed to download {url}: {err}"))?;
    let read_limit = limit
        .checked_add(1)
        .ok_or_else(|| "embedded UI download size limit overflowed".to_string())?;
    let body = response
        .body_mut()
        .with_config()
        .limit(read_limit)
        .read_to_vec()
        .map_err(|err| format!("failed to read {url}: {err}"))?;
    if body.len() as u64 > limit {
        return Err(format!(
            "download from {url} exceeds the {limit} byte limit"
        ));
    }
    Ok(body)
}

fn validate_https_url(url: &str) -> Result<(), String> {
    let uri = url
        .parse::<ureq::http::Uri>()
        .map_err(|err| format!("invalid embedded UI URL {url:?}: {err}"))?;
    if uri.scheme() != Some(&ureq::http::uri::Scheme::HTTPS) || uri.authority().is_none() {
        return Err(format!("refusing non-HTTPS embedded UI URL: {url}"));
    }
    Ok(())
}

fn parse_latest_tag(json: &[u8]) -> Result<String, String> {
    let release: serde_json::Value = serde_json::from_slice(json)
        .map_err(|err| format!("invalid GitHub latest-release response: {err}"))?;
    let tag = release
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .filter(|tag| !tag.is_empty())
        .ok_or_else(|| "GitHub latest-release response has no nonempty tag_name".to_string())?;
    validate_release_name(tag)?;
    Ok(tag.to_string())
}

fn parse_commit_sha(json: &[u8]) -> Result<String, String> {
    let commit: serde_json::Value = serde_json::from_slice(json)
        .map_err(|err| format!("invalid GitHub commit response: {err}"))?;
    let sha = commit
        .get("sha")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "GitHub commit response has no sha".to_string())?;
    if !is_sha_revision(sha) {
        return Err(
            "GitHub commit response sha must be exactly 40 hexadecimal characters".to_string(),
        );
    }
    Ok(sha.to_ascii_lowercase())
}

fn unpublished_error(revision: &str, detail: &str) -> String {
    format!(
        "embedded UI revision `{revision}` is not published as a querymt-desktop release: {detail}. Set QMT_UI_DIST to a local embedded UI directory/archive, use a plain QMT_UI_REVISION to build source, or use a valid content-pinned release selector"
    )
}

fn source_revision_error(revision: &str, detail: &str) -> String {
    format!(
        "failed to obtain querymt-desktop source revision `{revision}`: {detail}. Set QMT_UI_DIST to a local embedded UI directory/archive or set QMT_UI_REVISION to another Git SHA, tag, or ref"
    )
}

pub(crate) fn parse_checksum_sidecar(sidecar: &[u8]) -> Result<[u8; 32], String> {
    let sidecar = std::str::from_utf8(sidecar)
        .map_err(|err| format!("checksum sidecar is not UTF-8: {err}"))?;
    let field = sidecar
        .split_whitespace()
        .next()
        .ok_or_else(|| "checksum sidecar is empty".to_string())?;
    if field.len() != 64 || !field.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(
            "checksum sidecar must start with exactly 64 hexadecimal characters".to_string(),
        );
    }

    let mut checksum = [0_u8; 32];
    hex::decode_to_slice(field, &mut checksum)
        .map_err(|err| format!("checksum sidecar contains invalid SHA-256: {err}"))?;
    Ok(checksum)
}

pub(crate) fn verify_sha256(contents: &[u8], sidecar: &[u8]) -> Result<(), String> {
    let expected = parse_checksum_sidecar(sidecar)?;
    verify_digest(contents, &expected)
}

fn verify_digest(contents: &[u8], expected: &[u8; 32]) -> Result<(), String> {
    let actual: [u8; 32] = Sha256::digest(contents).into();
    if actual != *expected {
        return Err(format!(
            "SHA-256 mismatch: expected {}, got {}",
            hex::encode(expected),
            hex::encode(actual)
        ));
    }
    Ok(())
}

pub(crate) fn extract_archive(archive: &[u8], destination: &Path) -> Result<(), String> {
    extract_archive_with_limits(archive, destination, ARCHIVE_LIMITS)
}

pub(crate) fn extract_source_archive(
    archive: &[u8],
    destination: &Path,
) -> Result<PathBuf, String> {
    extract_source_archive_with_limits(archive, destination, SOURCE_ARCHIVE_LIMITS)
}

fn preflight_source_archive(archive: &[u8], limits: ArchiveLimits) -> Result<(), String> {
    if archive.len() as u64 > limits.compressed {
        return Err(format!(
            "querymt-desktop source archive exceeds the {} byte compressed size limit",
            limits.compressed
        ));
    }

    let decoder = GzDecoder::new(Cursor::new(archive));
    let decoder = LimitedReader::new(
        decoder,
        limits.decompressed,
        "querymt-desktop source decompressed stream",
    );
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|err| format!("failed to read querymt-desktop source tar archive: {err}"))?
        .raw(true);
    let mut entry_count = 0_u64;

    for entry in entries {
        entry_count = entry_count
            .checked_add(1)
            .ok_or_else(|| "querymt-desktop source archive entry count overflowed".to_string())?;
        if entry_count > limits.entries {
            return Err(format!(
                "querymt-desktop source archive exceeds the {} raw entry limit",
                limits.entries
            ));
        }

        let mut entry =
            entry.map_err(|err| format!("failed to read raw source tar entry: {err}"))?;
        let raw_path = entry.path_bytes();
        if raw_path.len() > limits.path_length {
            return Err(format!(
                "querymt-desktop source archive raw path exceeds the {} byte limit",
                limits.path_length
            ));
        }

        let entry_type = entry.header().entry_type();
        let is_extension_metadata = entry_type.is_pax_local_extensions()
            || entry_type.is_pax_global_extensions()
            || entry_type.is_gnu_longname()
            || entry_type.is_gnu_longlink();
        let entry_size = entry.size();
        if is_extension_metadata && entry_size > MAX_METADATA_SIZE {
            return Err(format!(
                "querymt-desktop source archive extension metadata exceeds the {MAX_METADATA_SIZE} byte limit"
            ));
        }

        let copied = io::copy(&mut entry, &mut io::sink())
            .map_err(|err| format!("failed to consume raw source tar entry: {err}"))?;
        if copied != entry_size {
            return Err(format!(
                "raw source archive entry declared {entry_size} bytes but read {copied}"
            ));
        }
    }

    let mut decoder = archive.into_inner();
    io::copy(&mut decoder, &mut io::sink())
        .map_err(|err| format!("failed to consume querymt-desktop source archive: {err}"))?;
    Ok(())
}

fn extract_source_archive_with_limits(
    archive: &[u8],
    destination: &Path,
    limits: ArchiveLimits,
) -> Result<PathBuf, String> {
    preflight_source_archive(archive, limits)?;

    let decoder = GzDecoder::new(Cursor::new(archive));
    let decoder = LimitedReader::new(
        decoder,
        limits.decompressed,
        "querymt-desktop source decompressed stream",
    );
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|err| format!("failed to read querymt-desktop source tar archive: {err}"))?;
    let mut extracted_size = 0_u64;
    let mut entry_count = 0_u64;
    let mut portable_paths = HashSet::new();
    let mut wrapper = None;

    for entry in entries {
        entry_count = entry_count
            .checked_add(1)
            .ok_or_else(|| "querymt-desktop source archive entry count overflowed".to_string())?;
        if entry_count > limits.entries {
            return Err(format!(
                "querymt-desktop source archive exceeds the {} entry limit",
                limits.entries
            ));
        }

        let mut entry = entry.map_err(|err| format!("failed to read source tar entry: {err}"))?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_pax_global_extensions() || entry_type.is_pax_local_extensions() {
            let entry_size = entry.size();
            if entry_size > MAX_METADATA_SIZE {
                return Err(format!(
                    "querymt-desktop source archive PAX metadata exceeds the {MAX_METADATA_SIZE} byte limit"
                ));
            }
            let copied = io::copy(&mut entry, &mut io::sink())
                .map_err(|err| format!("failed to read source archive PAX metadata: {err}"))?;
            if copied != entry_size {
                return Err(format!(
                    "source archive PAX metadata declared {entry_size} bytes but read {copied}"
                ));
            }
            continue;
        }
        let raw_path = entry.path_bytes();
        if raw_path.len() > limits.path_length {
            return Err(format!(
                "querymt-desktop source archive path exceeds the {} byte limit",
                limits.path_length
            ));
        }
        reject_source_archive_path(&raw_path, entry_type.is_dir())?;
        let portable_key = source_portable_path_key(&raw_path)?;
        if !portable_paths.insert(portable_key) {
            return Err(format!(
                "querymt-desktop source archive contains a duplicate or case-fold-colliding output path: {}",
                String::from_utf8_lossy(&raw_path)
            ));
        }

        let entry_path = entry
            .path()
            .map_err(|err| format!("invalid path in querymt-desktop source archive: {err}"))?;
        let relative_path = clean_relative_path(&entry_path)?;
        let first_component = relative_path
            .components()
            .next()
            .and_then(|component| match component {
                Component::Normal(component) => Some(PathBuf::from(component)),
                _ => None,
            })
            .ok_or_else(|| {
                "querymt-desktop source archive entry has no wrapper directory".to_string()
            })?;
        if let Some(expected) = &wrapper {
            if expected != &first_component {
                return Err(
                    "querymt-desktop source archive must contain exactly one common top-level directory"
                        .to_string(),
                );
            }
        } else {
            wrapper = Some(first_component.clone());
        }
        if relative_path == first_component && !entry_type.is_dir() {
            return Err(
                "querymt-desktop source archive top-level component must be a directory"
                    .to_string(),
            );
        }

        let output_path = destination.join(&relative_path);
        if entry_type.is_dir() {
            fs::create_dir_all(&output_path).map_err(|err| {
                format!(
                    "failed to create source archive directory {}: {err}",
                    output_path.display()
                )
            })?;
            continue;
        }
        if !entry_type.is_file() {
            return Err(format!(
                "querymt-desktop source archive entry {} is not a regular file or directory",
                relative_path.display()
            ));
        }

        let entry_size = entry.size();
        extracted_size = extracted_size
            .checked_add(entry_size)
            .ok_or_else(|| "querymt-desktop source extracted size overflowed".to_string())?;
        if extracted_size > limits.extracted {
            return Err(format!(
                "querymt-desktop source archive exceeds the {} byte extracted size limit",
                limits.extracted
            ));
        }

        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "failed to create source archive parent directory {}: {err}",
                    parent.display()
                )
            })?;
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output_path)
            .map_err(|err| {
                if err.kind() == io::ErrorKind::AlreadyExists {
                    format!(
                        "querymt-desktop source archive output collision: extracted file {} already exists",
                        output_path.display()
                    )
                } else {
                    format!(
                        "failed to create extracted source file {}: {err}",
                        output_path.display()
                    )
                }
            })?;
        let copied = io::copy(&mut entry, &mut output).map_err(|err| {
            format!(
                "failed to extract source archive file {}: {err}",
                output_path.display()
            )
        })?;
        if copied != entry_size {
            return Err(format!(
                "source archive file {} declared {entry_size} bytes but extracted {copied}",
                relative_path.display()
            ));
        }
    }

    let wrapper = wrapper.ok_or_else(|| "querymt-desktop source archive is empty".to_string())?;
    let source_directory = destination.join(wrapper);
    let metadata = fs::symlink_metadata(&source_directory).map_err(|err| {
        format!(
            "failed to inspect extracted source directory {}: {err}",
            source_directory.display()
        )
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!(
            "querymt-desktop source archive wrapper must be a regular directory: {}",
            source_directory.display()
        ));
    }
    Ok(source_directory)
}

fn reject_source_archive_path(path: &[u8], is_directory: bool) -> Result<(), String> {
    let path = std::str::from_utf8(path)
        .map_err(|err| format!("querymt-desktop source archive path is not UTF-8: {err}"))?;
    if path.starts_with('/') || path.starts_with('\\') {
        return Err("querymt-desktop source archive contains an absolute path".to_string());
    }
    if path.contains('\\') {
        return Err(
            "querymt-desktop source archive path contains a Windows path separator".to_string(),
        );
    }
    if path == "." || path == "./" {
        return if is_directory {
            Ok(())
        } else {
            Err("querymt-desktop source archive path has no output component".to_string())
        };
    }

    let mut components = path.split('/').peekable();
    let mut saw_normal_component = false;
    let mut component_index = 0;
    while let Some(component) = components.next() {
        if component.is_empty() {
            if components.peek().is_none() && saw_normal_component && is_directory {
                break;
            }
            return Err(
                "querymt-desktop source archive path contains an empty path component".to_string(),
            );
        }
        if component == ".." {
            return Err(
                "querymt-desktop source archive contains a parent path component".to_string(),
            );
        }
        if component == "." {
            if component_index == 0 {
                component_index += 1;
                continue;
            }
            return Err(
                "querymt-desktop source archive path contains a non-leading current-directory component"
                    .to_string(),
            );
        }
        if component_index == 0
            && component.len() == 2
            && component.as_bytes()[0].is_ascii_alphabetic()
            && component.as_bytes()[1] == b':'
        {
            return Err(
                "querymt-desktop source archive contains a Windows drive prefix".to_string(),
            );
        }
        if component.contains(':') {
            return Err("querymt-desktop source archive path contains a colon".to_string());
        }
        if component.ends_with('.') || component.ends_with(' ') {
            return Err(
                "querymt-desktop source archive path component has a trailing dot or space"
                    .to_string(),
            );
        }
        if is_windows_reserved_component(component.as_bytes()) {
            return Err(
                "querymt-desktop source archive path contains a reserved Windows device name"
                    .to_string(),
            );
        }

        saw_normal_component = true;
        component_index += 1;
    }

    if !saw_normal_component {
        return Err("querymt-desktop source archive path has no output component".to_string());
    }
    Ok(())
}

fn source_portable_path_key(path: &[u8]) -> Result<String, String> {
    let path = std::str::from_utf8(path)
        .map_err(|err| format!("querymt-desktop source archive path is not UTF-8: {err}"))?;
    Ok(path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join("/"))
}

fn extract_archive_with_limits(
    archive: &[u8],
    destination: &Path,
    limits: ArchiveLimits,
) -> Result<(), String> {
    if archive.len() as u64 > limits.compressed {
        return Err(format!(
            "embedded UI archive exceeds the {} byte compressed size limit",
            limits.compressed
        ));
    }

    let decoder = GzDecoder::new(Cursor::new(archive));
    let decoder = LimitedReader::new(
        decoder,
        limits.decompressed,
        "embedded UI decompressed stream",
    );
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|err| format!("failed to read embedded UI tar archive: {err}"))?;
    let mut extracted_size = 0_u64;
    let mut entry_count = 0_u64;
    let mut portable_paths = HashSet::new();

    for entry in entries {
        entry_count = entry_count
            .checked_add(1)
            .ok_or_else(|| "embedded UI archive entry count overflowed".to_string())?;
        if entry_count > limits.entries {
            return Err(format!(
                "embedded UI archive exceeds the {} entry limit",
                limits.entries
            ));
        }

        let mut entry = entry.map_err(|err| format!("failed to read tar entry: {err}"))?;
        let entry_type = entry.header().entry_type();
        let raw_path = entry.path_bytes();
        if raw_path.len() > limits.path_length {
            return Err(format!(
                "embedded UI archive path exceeds the {} byte limit",
                limits.path_length
            ));
        }
        reject_portable_unsafe_path(&raw_path, entry_type.is_dir())?;
        let portable_key = portable_path_key(&raw_path)?;
        if !portable_paths.insert(portable_key) {
            return Err(format!(
                "embedded UI archive contains a duplicate or case-fold-colliding output path: {}",
                String::from_utf8_lossy(&raw_path)
            ));
        }

        let entry_path = entry
            .path()
            .map_err(|err| format!("invalid path in embedded UI archive: {err}"))?;
        let relative_path = clean_relative_path(&entry_path)?;

        if relative_path.as_os_str().is_empty() {
            if entry_type.is_dir() {
                continue;
            }
            return Err("embedded UI archive contains an empty file path".to_string());
        }

        let output_path = destination.join(&relative_path);
        if entry_type.is_dir() {
            fs::create_dir_all(&output_path).map_err(|err| {
                format!(
                    "failed to create archive directory {}: {err}",
                    output_path.display()
                )
            })?;
            continue;
        }
        if !entry_type.is_file() {
            return Err(format!(
                "embedded UI archive entry {} is not a regular file or directory",
                relative_path.display()
            ));
        }

        let entry_size = entry.size();
        extracted_size = extracted_size
            .checked_add(entry_size)
            .ok_or_else(|| "embedded UI extracted size overflowed".to_string())?;
        if extracted_size > limits.extracted {
            return Err(format!(
                "embedded UI archive exceeds the {} byte extracted size limit",
                limits.extracted
            ));
        }

        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "failed to create archive parent directory {}: {err}",
                    parent.display()
                )
            })?;
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output_path)
            .map_err(|err| {
                if err.kind() == io::ErrorKind::AlreadyExists {
                    format!(
                        "embedded UI archive output collision: extracted file {} already exists",
                        output_path.display()
                    )
                } else {
                    format!(
                        "failed to create extracted file {}: {err}",
                        output_path.display()
                    )
                }
            })?;
        let copied = io::copy(&mut entry, &mut output).map_err(|err| {
            format!(
                "failed to extract archive file {}: {err}",
                output_path.display()
            )
        })?;
        if copied != entry_size {
            return Err(format!(
                "archive file {} declared {entry_size} bytes but extracted {copied}",
                relative_path.display()
            ));
        }
    }

    Ok(())
}

struct LimitedReader<R> {
    inner: R,
    remaining: u64,
    description: &'static str,
}

impl<R> LimitedReader<R> {
    fn new(inner: R, limit: u64, description: &'static str) -> Self {
        Self {
            inner,
            remaining: limit,
            description,
        }
    }
}

impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let maximum = self.remaining.saturating_add(1).min(buffer.len() as u64) as usize;
        let read = self.inner.read(&mut buffer[..maximum])?;
        if read as u64 > self.remaining {
            self.remaining = 0;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} exceeds its byte limit", self.description),
            ));
        }
        self.remaining -= read as u64;
        Ok(read)
    }
}

fn copy_directory(source: &Path, destination: &Path) -> Result<(), String> {
    let mut entries = fs::read_dir(source)
        .map_err(|err| format!("failed to read UI directory {}: {err}", source.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| {
            format!(
                "failed to enumerate UI directory {}: {err}",
                source.display()
            )
        })?;
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path).map_err(|err| {
            format!(
                "failed to inspect UI directory entry {}: {err}",
                source_path.display()
            )
        })?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Err(format!(
                "QMT_UI_DIST contains a symlink, which is not allowed: {}",
                source_path.display()
            ));
        }
        if metadata.is_dir() {
            fs::create_dir(&destination_path).map_err(|err| {
                format!(
                    "failed to create staged UI directory {}: {err}",
                    destination_path.display()
                )
            })?;
            copy_directory(&source_path, &destination_path)?;
        } else if metadata.is_file() {
            fs::copy(&source_path, &destination_path).map_err(|err| {
                format!(
                    "failed to copy UI file {} to {}: {err}",
                    source_path.display(),
                    destination_path.display()
                )
            })?;
        } else {
            return Err(format!(
                "QMT_UI_DIST contains a non-file, non-directory entry: {}",
                source_path.display()
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_manifest(
    directory: &Path,
    expected_revision: Option<&str>,
) -> Result<(), String> {
    let index_path = directory.join("index.html");
    if !is_regular_file(&index_path)? {
        return Err(format!(
            "embedded UI must contain a regular index.html: {}",
            index_path.display()
        ));
    }

    let manifest_path = directory.join("querymt-ui.json");
    if !is_regular_file(&manifest_path)? {
        return Err(format!(
            "embedded UI must contain a regular querymt-ui.json: {}",
            manifest_path.display()
        ));
    }
    let manifest = read_file_limited(&manifest_path, MAX_METADATA_SIZE, "embedded UI manifest")?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest)
        .map_err(|err| format!("invalid {}: {err}", manifest_path.display()))?;

    let target = manifest.get("target").and_then(serde_json::Value::as_str);
    if target != Some("embedded") {
        return Err(format!(
            "querymt-ui.json target must be 'embedded', got {target:?}"
        ));
    }
    let acp_path = manifest
        .get("acpWebSocketPath")
        .and_then(serde_json::Value::as_str);
    if acp_path != Some("/acp/ws") {
        return Err(format!(
            "querymt-ui.json acpWebSocketPath must be '/acp/ws', got {acp_path:?}"
        ));
    }
    let revision = manifest
        .get("revision")
        .and_then(serde_json::Value::as_str)
        .filter(|revision| !revision.is_empty())
        .ok_or_else(|| "querymt-ui.json revision must be a nonempty string".to_string())?;
    if let Some(expected) = expected_revision
        && revision != expected
    {
        return Err(format!(
            "querymt-ui.json revision must equal resolved source commit {expected}, got {revision}"
        ));
    }

    Ok(())
}

fn is_regular_file(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && !metadata.file_type().is_symlink()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(format!("failed to inspect {}: {err}", path.display())),
    }
}

fn read_file_limited(path: &Path, limit: u64, description: &str) -> Result<Vec<u8>, String> {
    let metadata = fs::metadata(path)
        .map_err(|err| format!("failed to inspect {description} {}: {err}", path.display()))?;
    if metadata.len() > limit {
        return Err(format!(
            "{description} {} exceeds the {limit} byte limit",
            path.display()
        ));
    }
    fs::read(path).map_err(|err| format!("failed to read {description} {}: {err}", path.display()))
}

fn recover_stale_backup(destination: &Path, backup: &Path) -> Result<(), String> {
    if !path_exists(destination)? && path_exists(backup)? {
        fs::rename(backup, destination).map_err(|err| {
            format!(
                "failed to restore stale embedded UI backup {} to {}: {err}",
                backup.display(),
                destination.display()
            )
        })?;
    }
    Ok(())
}

fn swap_staging_directory(staging: &Path, destination: &Path, backup: &Path) -> Result<(), String> {
    let had_destination = path_exists(destination)?;
    if had_destination {
        fs::rename(destination, backup).map_err(|err| {
            format!(
                "failed to move previous embedded UI directory {} aside: {err}",
                destination.display()
            )
        })?;
    }

    if let Err(activation_error) = fs::rename(staging, destination) {
        if had_destination {
            if let Err(rollback_error) = fs::rename(backup, destination) {
                return Err(format!(
                    "failed to activate embedded UI staging directory {}: {activation_error}; rollback also failed: {rollback_error}",
                    destination.display()
                ));
            }
            return Err(format!(
                "failed to activate embedded UI staging directory {}: {activation_error}; previous embedded UI was restored",
                destination.display()
            ));
        }
        return Err(format!(
            "failed to activate embedded UI staging directory {}: {activation_error}; no previous embedded UI existed",
            destination.display()
        ));
    }

    if had_destination {
        remove_path_if_exists(backup)?;
    }
    Ok(())
}

fn path_exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(format!("failed to inspect {}: {err}", path.display())),
    }
}

fn remove_path_if_exists(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)
                .map_err(|err| format!("failed to remove directory {}: {err}", path.display()))
        }
        Ok(_) => fs::remove_file(path)
            .map_err(|err| format!("failed to remove file {}: {err}", path.display())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("failed to inspect {}: {err}", path.display())),
    }
}

fn clean_relative_path(path: &Path) -> Result<PathBuf, String> {
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(component) => clean.push(component),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "embedded UI archive contains unsafe path {}",
                    path.display()
                ));
            }
        }
    }
    Ok(clean)
}

fn reject_portable_unsafe_path(path: &[u8], is_directory: bool) -> Result<(), String> {
    if path.starts_with(b"/") || path.starts_with(b"\\") {
        return Err("embedded UI archive contains an absolute path".to_string());
    }
    if path.contains(&b'\\') {
        return Err("embedded UI archive path contains a Windows path separator".to_string());
    }
    if !path.is_ascii() {
        return Err("embedded UI archive path contains a non-ASCII byte".to_string());
    }
    if path == b"." || path == b"./" {
        return if is_directory {
            Ok(())
        } else {
            Err("embedded UI archive path has no output component".to_string())
        };
    }

    let mut components = path.split(|byte| *byte == b'/').peekable();
    let mut saw_normal_component = false;
    let mut component_index = 0;
    while let Some(component) = components.next() {
        if component.is_empty() {
            if components.peek().is_none() && saw_normal_component && is_directory {
                break;
            }
            return Err("embedded UI archive path contains an empty path component".to_string());
        }
        if component == b".." {
            return Err("embedded UI archive contains a parent path component".to_string());
        }
        if component == b"." {
            if component_index == 0 {
                component_index += 1;
                continue;
            }
            return Err(
                "embedded UI archive path contains a non-leading current-directory component"
                    .to_string(),
            );
        }
        if component_index == 0
            && component.len() == 2
            && component[0].is_ascii_alphabetic()
            && component[1] == b':'
        {
            return Err("embedded UI archive contains a Windows drive prefix".to_string());
        }
        if component.contains(&b':') {
            return Err("embedded UI archive path contains a colon".to_string());
        }
        if component.ends_with(b".") || component.ends_with(b" ") {
            return Err(
                "embedded UI archive path component has a trailing dot or space".to_string(),
            );
        }
        if is_windows_reserved_component(component) {
            return Err(
                "embedded UI archive path contains a reserved Windows device name".to_string(),
            );
        }

        saw_normal_component = true;
        component_index += 1;
    }

    if !saw_normal_component {
        return Err("embedded UI archive path has no output component".to_string());
    }
    Ok(())
}

fn is_windows_reserved_component(component: &[u8]) -> bool {
    let base = component
        .split(|byte| *byte == b'.')
        .next()
        .unwrap_or(component);

    base.eq_ignore_ascii_case(b"CON")
        || base.eq_ignore_ascii_case(b"PRN")
        || base.eq_ignore_ascii_case(b"AUX")
        || base.eq_ignore_ascii_case(b"NUL")
        || (base.len() == 4
            && (base[..3].eq_ignore_ascii_case(b"COM") || base[..3].eq_ignore_ascii_case(b"LPT"))
            && matches!(base[3], b'1'..=b'9'))
}

fn portable_path_key(path: &[u8]) -> Result<String, String> {
    if !path.is_ascii() {
        return Err("embedded UI archive path must contain only ASCII bytes".to_string());
    }
    let path = std::str::from_utf8(path)
        .map_err(|_| "embedded UI archive path must contain only ASCII bytes".to_string())?;
    Ok(path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join("/"))
}

fn sidecar_path(archive: &Path) -> PathBuf {
    let mut sidecar = archive.as_os_str().to_os_string();
    sidecar.push(".sha256");
    PathBuf::from(sidecar)
}

fn has_tar_gz_suffix(path: &Path) -> bool {
    path.file_name()
        .map(|name| name.to_string_lossy().ends_with(".tar.gz"))
        .unwrap_or(false)
}

fn is_sha_revision(revision: &str) -> bool {
    revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn percent_encode_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::{Builder, EntryType, Header};
    use tempfile::TempDir;

    const FIXTURE_REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
    const FIXTURE_DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn revision_selection_distinguishes_sources_and_content_pinned_releases() {
        for source in [
            "latest",
            "sha256-migration",
            "feature@sha256-fix",
            "v1@sha256",
        ] {
            assert_eq!(
                select_revision(source).unwrap(),
                RevisionSpec::Source(source.to_string())
            );
        }

        assert_eq!(
            select_revision(&format!("sha256:{FIXTURE_DIGEST}")).unwrap(),
            RevisionSpec::Release {
                selector: ReleaseSelector::Latest,
                archive_sha256: parse_digest(FIXTURE_DIGEST).unwrap(),
            }
        );
        assert_eq!(
            select_revision(&format!("v1.2.3@sha256:{FIXTURE_DIGEST}")).unwrap(),
            RevisionSpec::Release {
                selector: ReleaseSelector::Release("v1.2.3".to_string()),
                archive_sha256: parse_digest(FIXTURE_DIGEST).unwrap(),
            }
        );
        assert_eq!(
            select_revision(&format!("latest@sha256:{FIXTURE_DIGEST}")).unwrap(),
            RevisionSpec::Release {
                selector: ReleaseSelector::Latest,
                archive_sha256: parse_digest(FIXTURE_DIGEST).unwrap(),
            }
        );

        let commit_shaped_source = "a".repeat(64);
        assert_eq!(
            select_revision(&commit_shaped_source).unwrap(),
            RevisionSpec::Source(commit_shaped_source)
        );
        assert!(select_revision("sha256:1234").is_err());
        assert!(select_revision("v1@sha256:1234").is_err());
    }

    #[test]
    fn github_responses_require_valid_tag_and_commit() {
        assert_eq!(
            parse_latest_tag(br#"{"tag_name":"v2.0.0"}"#).unwrap(),
            "v2.0.0"
        );
        assert!(parse_latest_tag(br#"{"tag_name":""}"#).is_err());
        let response = format!(
            r#"{{"sha":"{}","other":true}}"#,
            FIXTURE_REVISION.to_ascii_uppercase()
        );
        assert_eq!(
            parse_commit_sha(response.as_bytes()).unwrap(),
            FIXTURE_REVISION
        );
        assert!(parse_commit_sha(br#"{"sha":"short"}"#).is_err());
        assert!(parse_commit_sha(br#"{"message":"not found"}"#).is_err());
    }

    #[test]
    fn extracts_valid_archive() {
        let temp = TempDir::new().unwrap();
        let archive = valid_archive(FIXTURE_REVISION);
        extract_archive(&archive, temp.path()).unwrap();
        validate_manifest(temp.path(), Some(FIXTURE_REVISION)).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("_app/app.js")).unwrap(),
            "console.log('ok');"
        );
    }

    #[test]
    fn extracts_source_archive_wrapper_with_pax_metadata_and_unicode_paths() {
        let temp = TempDir::new().unwrap();
        let archive = source_archive_with_global_pax(&[
            ("querymt-desktop-revision/package.json", b"{}"),
            (
                "querymt-desktop-revision/src/umlaut-\u{00e4}.ts",
                b"export {};",
            ),
        ]);

        let source = extract_source_archive(&archive, temp.path()).unwrap();

        assert_eq!(source, temp.path().join("querymt-desktop-revision"));
        assert!(source.join("package.json").is_file());
        assert!(source.join("src/umlaut-\u{00e4}.ts").is_file());
    }

    fn assert_oversized_source_metadata_is_rejected(entry_type: EntryType) {
        let metadata = vec![b'x'; MAX_METADATA_SIZE as usize + 1];
        let archive = archive_with_raw_entries(&[
            (b"metadata", entry_type, metadata.as_slice()),
            (b"wrapper/file", EntryType::Regular, b"ok"),
        ]);
        let temp = TempDir::new().unwrap();

        let error = extract_source_archive(&archive, temp.path()).unwrap_err();

        assert_eq!(
            error,
            format!(
                "querymt-desktop source archive extension metadata exceeds the {MAX_METADATA_SIZE} byte limit"
            )
        );
        assert!(fs::read_dir(temp.path()).unwrap().next().is_none());
    }

    #[test]
    fn rejects_oversized_source_metadata_during_raw_preflight() {
        for entry_type in [EntryType::XHeader, EntryType::GNULongName] {
            assert_oversized_source_metadata_is_rejected(entry_type);
        }
    }

    #[test]
    fn source_archive_requires_one_safe_wrapper_and_regular_entries() {
        let multiple_roots = archive_with_files(&[("one/a", b"a"), ("two/b", b"b")]);
        let temp = TempDir::new().unwrap();
        assert!(
            extract_source_archive(&multiple_roots, temp.path())
                .unwrap_err()
                .contains("one common top-level directory")
        );

        let traversal = archive_with_raw_path(b"wrapper/../../outside", EntryType::Regular, b"bad");
        let temp = TempDir::new().unwrap();
        assert!(extract_source_archive(&traversal, temp.path()).is_err());
        assert!(!temp.path().parent().unwrap().join("outside").exists());

        let symlink = archive_with_link("wrapper/link", EntryType::Symlink);
        let temp = TempDir::new().unwrap();
        assert!(
            extract_source_archive(&symlink, temp.path())
                .unwrap_err()
                .contains("not a regular file or directory")
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_build_runs_required_npm_commands_and_stages_artifact() {
        use std::os::unix::fs::PermissionsExt;

        const SENTINEL_SECRET: &str = "QUERYMT_TEST_SENTINEL_SECRET";

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let staging = temp.path().join("staging");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&staging).unwrap();
        let npm = temp.path().join("fake-npm");
        fs::write(
            &npm,
            r#"#!/bin/sh
set -eu
printf '%s|%s\n' "$QUERYMT_UI_REVISION" "$*" >> npm-calls
if [ "${QUERYMT_TEST_SENTINEL_SECRET+x}" = x ]; then
  printf 'secret=present\n' > npm-env
else
  printf 'secret=absent\n' > npm-env
fi
printf 'revision=%s\n' "$QUERYMT_UI_REVISION" >> npm-env
printf 'path=%s\n' "$PATH" >> npm-env
printf 'home=%s\n' "$HOME" >> npm-env
printf 'userprofile=%s\n' "$USERPROFILE" >> npm-env
printf 'xdg_cache=%s\n' "$XDG_CACHE_HOME" >> npm-env
printf 'npm_cache=%s\n' "$npm_config_cache" >> npm-env
printf 'ci=%s\n' "$CI" >> npm-env
if [ "$*" = "run build:embedded" ]; then
  mkdir -p build-embedded/_app
  printf '<!doctype html>\n' > build-embedded/index.html
  printf 'console.log("ok");\n' > build-embedded/_app/app.js
  printf '{"target":"embedded","acpWebSocketPath":"/acp/ws","revision":"%s"}\n' "$QUERYMT_UI_REVISION" > build-embedded/querymt-ui.json
fi
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&npm).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&npm, permissions).unwrap();

        // SAFETY: this test uses a unique variable that no other test reads or writes.
        unsafe { std::env::set_var(SENTINEL_SECRET, "must-not-leak") };
        let build_result =
            build_source_artifact(&source, &staging, FIXTURE_REVISION, npm.as_os_str());
        // SAFETY: restores the unique test-only variable immediately after spawning npm.
        unsafe { std::env::remove_var(SENTINEL_SECRET) };
        build_result.unwrap();

        assert_eq!(
            fs::read_to_string(source.join("npm-calls")).unwrap(),
            format!(
                "{FIXTURE_REVISION}|ci\n{FIXTURE_REVISION}|run test:embedded\n{FIXTURE_REVISION}|run build:embedded\n"
            )
        );
        let path = std::env::var("PATH").unwrap();
        assert_eq!(
            fs::read_to_string(source.join("npm-env")).unwrap(),
            format!(
                "secret=absent\nrevision={FIXTURE_REVISION}\npath={path}\nhome={}\nuserprofile={}\nxdg_cache={}\nnpm_cache={}\nci=true\n",
                source.join(".querymt-build-home").display(),
                source.join(".querymt-build-home").display(),
                source.join(".querymt-npm-cache").display(),
                source.join(".querymt-npm-cache").display(),
            )
        );
        assert!(source.join(".querymt-build-home").is_dir());
        assert!(source.join(".querymt-npm-cache").is_dir());
        validate_manifest(&staging, Some(FIXTURE_REVISION)).unwrap();

        let error = run_npm_command(
            &source,
            FIXTURE_REVISION,
            temp.path().join("missing-npm").as_os_str(),
            &["ci"],
        )
        .unwrap_err();
        assert!(error.contains("npm is required"), "{error}");
    }

    #[test]
    fn rejects_malformed_archive() {
        let temp = TempDir::new().unwrap();
        let error = extract_archive(b"not a gzip archive", temp.path()).unwrap_err();
        assert!(error.contains("failed to read"), "{error}");
    }

    #[test]
    fn rejects_archive_traversal() {
        let temp = TempDir::new().unwrap();
        let archive = archive_with_raw_path(b"../outside", EntryType::Regular, b"bad");
        let error = extract_archive(&archive, temp.path()).unwrap_err();
        assert!(error.contains("parent path") || error.contains("unsafe path"));
        assert!(!temp.path().parent().unwrap().join("outside").exists());
    }

    #[test]
    fn rejects_archive_symlinks_and_hardlinks() {
        for entry_type in [EntryType::Symlink, EntryType::Link] {
            let temp = TempDir::new().unwrap();
            let archive = archive_with_link("link", entry_type);
            let error = extract_archive(&archive, temp.path()).unwrap_err();
            assert!(error.contains("not a regular file or directory"));
        }
    }

    #[test]
    fn rejects_decompressed_stream_limit() {
        let archive = archive_with_files(&[("a", b""), ("b", b""), ("c", b"")]);
        let temp = TempDir::new().unwrap();
        let error = extract_archive_with_limits(
            &archive,
            temp.path(),
            ArchiveLimits {
                decompressed: 511,
                ..ARCHIVE_LIMITS
            },
        )
        .unwrap_err();
        assert!(error.contains("decompressed stream"), "{error}");
    }

    #[test]
    fn rejects_duplicate_and_case_fold_colliding_archive_paths() {
        for entries in [
            [("same", b"one".as_slice()), ("same", b"two".as_slice())],
            [("App.js", b"one".as_slice()), ("app.js", b"two".as_slice())],
        ] {
            let archive = archive_with_files(&entries);
            let temp = TempDir::new().unwrap();
            let error = extract_archive(&archive, temp.path()).unwrap_err();
            assert!(error.contains("case-fold-colliding"), "{error}");
        }
    }

    #[test]
    fn rejects_ads_and_non_ascii_archive_paths() {
        let cases: &[(&[u8], &str)] = &[
            (b"index.html::$DATA", "colon"),
            (b"\xC3\x84.js", "non-ASCII"),
        ];

        for &(path, expected_error) in cases {
            let archive = archive_with_raw_path(path, EntryType::Regular, b"bad");
            let temp = TempDir::new().unwrap();
            let error = extract_archive(&archive, temp.path()).unwrap_err();
            assert!(error.contains(expected_error), "{path:?}: {error}");
        }
    }

    #[test]
    fn rejects_windows_ambiguous_archive_components() {
        let cases: &[(&[u8], &str)] = &[
            (b"assets/app.js.", "trailing dot or space"),
            (b"assets/app.js ", "trailing dot or space"),
            (b"CON", "reserved Windows device name"),
            (b"prn.txt", "reserved Windows device name"),
            (b"assets/AuX.js", "reserved Windows device name"),
            (b"nul", "reserved Windows device name"),
            (b"COM1.js", "reserved Windows device name"),
            (b"com9", "reserved Windows device name"),
            (b"LPT1.css", "reserved Windows device name"),
            (b"lpt9", "reserved Windows device name"),
            (b"_app//immutable/app.js", "empty path component"),
            (b"assets/", "empty path component"),
        ];

        for &(path, expected_error) in cases {
            let archive = archive_with_raw_path(path, EntryType::Regular, b"bad");
            let temp = TempDir::new().unwrap();
            let error = extract_archive(&archive, temp.path()).unwrap_err();
            assert!(error.contains(expected_error), "{path:?}: {error}");
        }
    }

    #[test]
    fn extracts_tar_root_and_safe_generated_asset_paths() {
        let archive = archive_with_raw_entries(&[
            (b"./".as_slice(), EntryType::Directory, b"".as_slice()),
            (b"./_app/".as_slice(), EntryType::Directory, b"".as_slice()),
            (
                b"./_app/immutable/".as_slice(),
                EntryType::Directory,
                b"".as_slice(),
            ),
            (
                b"./_app/immutable/chunks/".as_slice(),
                EntryType::Directory,
                b"".as_slice(),
            ),
            (
                b"./_app/immutable/chunks/app.js".as_slice(),
                EntryType::Regular,
                b"console.log('safe');".as_slice(),
            ),
        ]);
        let temp = TempDir::new().unwrap();

        extract_archive(&archive, temp.path()).unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join("_app/immutable/chunks/app.js")).unwrap(),
            "console.log('safe');"
        );
    }

    #[test]
    fn refuses_to_overwrite_an_existing_output_file() {
        let temp = TempDir::new().unwrap();
        let output = temp.path().join("index.html");
        fs::write(&output, "original").unwrap();
        let archive = archive_with_files(&[("index.html", b"replacement")]);

        let error = extract_archive(&archive, temp.path()).unwrap_err();

        assert!(error.contains("output collision"), "{error}");
        assert_eq!(fs::read_to_string(output).unwrap(), "original");
    }

    #[test]
    fn validates_manifest_fields_and_source_commit() {
        let temp = TempDir::new().unwrap();
        write_fixture_directory(temp.path(), FIXTURE_REVISION);
        validate_manifest(temp.path(), Some(FIXTURE_REVISION)).unwrap();

        assert!(
            validate_manifest(
                temp.path(),
                Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            )
            .unwrap_err()
            .contains("resolved source commit")
        );
        fs::write(
            temp.path().join("querymt-ui.json"),
            r#"{"target":"embedded","acpWebSocketPath":"/wrong","revision":"tag-build-sha"}"#,
        )
        .unwrap();
        assert!(
            validate_manifest(temp.path(), None)
                .unwrap_err()
                .contains("acpWebSocketPath")
        );
    }

    #[test]
    fn rejects_bad_checksum() {
        let error = verify_sha256(
            b"archive",
            b"0000000000000000000000000000000000000000000000000000000000000000  archive.tar.gz\n",
        )
        .unwrap_err();
        assert!(error.contains("SHA-256 mismatch"));
        assert!(parse_checksum_sidecar(b"1234 archive.tar.gz").is_err());
    }

    #[test]
    fn stages_local_directory_without_network() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source with spaces");
        let out_dir = temp.path().join("target with spaces/out");
        fs::create_dir_all(&source).unwrap();
        write_fixture_directory(&source, FIXTURE_REVISION);

        let prepared = prepare_ui(
            &out_dir,
            Some(source.as_os_str()),
            Some("invalid remote syntax is ignored for local mode"),
            FIXTURE_REVISION,
        )
        .unwrap();
        assert_eq!(prepared.embed_dir, out_dir.join("querymt-ui"));
        assert_eq!(prepared.watched_paths, vec![source.clone()]);
        assert!(prepared.embed_dir.join("index.html").is_file());
        assert_eq!(
            prepared.source_log,
            format!("querymt-ui: source = local directory {}", source.display())
        );
    }

    #[test]
    fn rejects_local_directory_overlap_in_both_directions() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let out_inside_source = source.join("target/out");
        fs::create_dir_all(&source).unwrap();
        write_fixture_directory(&source, FIXTURE_REVISION);
        let error = prepare_ui(
            &out_inside_source,
            Some(source.as_os_str()),
            None,
            FIXTURE_REVISION,
        )
        .unwrap_err();
        assert!(error.contains("must not contain or overlap"), "{error}");

        let out_dir = temp.path().join("other-out");
        let source_inside_out = out_dir.join("source");
        fs::create_dir_all(&source_inside_out).unwrap();
        write_fixture_directory(&source_inside_out, FIXTURE_REVISION);
        let error = prepare_ui(
            &out_dir,
            Some(source_inside_out.as_os_str()),
            None,
            FIXTURE_REVISION,
        )
        .unwrap_err();
        assert!(error.contains("must not contain or overlap"), "{error}");
    }

    #[test]
    fn stages_local_archive_and_always_watches_optional_checksum() {
        let temp = TempDir::new().unwrap();
        let archive_path = temp.path().join("ui fixture.tar.gz");
        let sidecar = sidecar_path(&archive_path);
        let archive = valid_archive(FIXTURE_REVISION);
        fs::write(&archive_path, &archive).unwrap();
        let digest: [u8; 32] = Sha256::digest(&archive).into();
        fs::write(
            &sidecar,
            format!("{}  {}\n", hex::encode(digest), archive_path.display()),
        )
        .unwrap();

        let verified_out = temp.path().join("verified-out");
        let prepared = prepare_ui(
            &verified_out,
            Some(archive_path.as_os_str()),
            None,
            FIXTURE_REVISION,
        )
        .unwrap();
        assert!(prepared.source_log.contains("sidecar verified"));
        assert_eq!(
            prepared.watched_paths,
            vec![archive_path.clone(), sidecar.clone()]
        );
        assert!(prepared.embed_dir.join("_app/app.js").is_file());

        fs::remove_file(&sidecar).unwrap();
        let trusted_out = temp.path().join("trusted-out");
        let prepared = prepare_ui(
            &trusted_out,
            Some(archive_path.as_os_str()),
            None,
            FIXTURE_REVISION,
        )
        .unwrap();
        assert!(prepared.source_log.contains("checksum not supplied"));
        assert_eq!(prepared.watched_paths, vec![archive_path, sidecar]);
    }

    #[test]
    fn local_archive_rejects_bad_adjacent_checksum() {
        let temp = TempDir::new().unwrap();
        let archive_path = temp.path().join("ui.tar.gz");
        fs::write(&archive_path, valid_archive(FIXTURE_REVISION)).unwrap();
        fs::write(
            sidecar_path(&archive_path),
            "0000000000000000000000000000000000000000000000000000000000000000  ui.tar.gz\n",
        )
        .unwrap();

        let error = prepare_ui(
            temp.path(),
            Some(archive_path.as_os_str()),
            None,
            FIXTURE_REVISION,
        )
        .unwrap_err();
        assert!(error.contains("SHA-256 mismatch"));
    }

    #[test]
    fn restores_stale_backup_before_staging_and_rolls_back_activation_failure() {
        let temp = TempDir::new().unwrap();
        let destination = temp.path().join("querymt-ui");
        let backup = temp.path().join(".querymt-ui-backup");
        fs::create_dir(&backup).unwrap();
        fs::write(backup.join("marker"), "old").unwrap();
        recover_stale_backup(&destination, &backup).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("marker")).unwrap(),
            "old"
        );
        assert!(!backup.exists());

        let missing_staging = temp.path().join("missing-staging");
        let error = swap_staging_directory(&missing_staging, &destination, &backup).unwrap_err();
        assert!(
            error.contains("previous embedded UI was restored"),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(destination.join("marker")).unwrap(),
            "old"
        );
        assert!(!backup.exists());
    }

    #[cfg(unix)]
    #[test]
    fn local_directory_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let out_dir = temp.path().join("out");
        fs::create_dir(&source).unwrap();
        write_fixture_directory(&source, FIXTURE_REVISION);
        symlink("index.html", source.join("linked-index.html")).unwrap();

        let error =
            prepare_ui(&out_dir, Some(source.as_os_str()), None, FIXTURE_REVISION).unwrap_err();
        assert!(error.contains("contains a symlink"));
    }

    fn write_fixture_directory(directory: &Path, revision: &str) {
        fs::create_dir_all(directory.join("_app")).unwrap();
        fs::write(directory.join("index.html"), "<!doctype html>").unwrap();
        fs::write(directory.join("_app/app.js"), "console.log('ok');").unwrap();
        fs::write(
            directory.join("querymt-ui.json"),
            format!(
                r#"{{"target":"embedded","acpWebSocketPath":"/acp/ws","revision":"{revision}"}}"#
            ),
        )
        .unwrap();
    }

    fn valid_archive(revision: &str) -> Vec<u8> {
        let mut entries = vec![
            ("index.html", b"<!doctype html>".as_slice()),
            ("_app/app.js", b"console.log('ok');".as_slice()),
        ];
        let manifest = format!(
            r#"{{"target":"embedded","acpWebSocketPath":"/acp/ws","revision":"{revision}"}}"#
        );
        entries.push(("querymt-ui.json", manifest.as_bytes()));
        archive_with_files(&entries)
    }

    fn archive_with_files(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = Builder::new(encoder);
        append_files(&mut builder, entries);
        finish_archive(builder)
    }

    fn source_archive_with_global_pax(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = Builder::new(encoder);
        let metadata = b"19 comment=fixture\n";
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::XGlobalHeader);
        header.set_mode(0o644);
        header.set_size(metadata.len() as u64);
        header.set_path("pax_global_header").unwrap();
        header.set_cksum();
        builder.append(&header, metadata.as_slice()).unwrap();
        append_files(&mut builder, entries);
        finish_archive(builder)
    }

    fn append_files(builder: &mut Builder<GzEncoder<Vec<u8>>>, entries: &[(&str, &[u8])]) {
        for (path, contents) in entries {
            let mut header = Header::new_gnu();
            header.set_entry_type(EntryType::Regular);
            header.set_mode(0o644);
            header.set_size(contents.len() as u64);
            header.set_path(path).unwrap();
            header.set_cksum();
            builder.append(&header, *contents).unwrap();
        }
    }

    fn archive_with_raw_path(path: &[u8], entry_type: EntryType, contents: &[u8]) -> Vec<u8> {
        archive_with_raw_entries(&[(path, entry_type, contents)])
    }

    fn archive_with_raw_entries(entries: &[(&[u8], EntryType, &[u8])]) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = Builder::new(encoder);
        for &(path, entry_type, contents) in entries {
            let mut header = Header::new_gnu();
            header.set_entry_type(entry_type);
            header.set_mode(0o644);
            header.set_size(contents.len() as u64);
            header.as_mut_bytes()[..100].fill(0);
            header.as_mut_bytes()[..path.len()].copy_from_slice(path);
            header.set_cksum();
            builder.append(&header, contents).unwrap();
        }
        finish_archive(builder)
    }

    fn archive_with_link(path: &str, entry_type: EntryType) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = Builder::new(encoder);
        let mut header = Header::new_gnu();
        header.set_entry_type(entry_type);
        header.set_mode(0o777);
        header.set_size(0);
        header.set_path(path).unwrap();
        header.set_link_name("index.html").unwrap();
        header.set_cksum();
        builder.append(&header, io::empty()).unwrap();
        finish_archive(builder)
    }

    fn finish_archive(mut builder: Builder<GzEncoder<Vec<u8>>>) -> Vec<u8> {
        builder.finish().unwrap();
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap()
    }
}

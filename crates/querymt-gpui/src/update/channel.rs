//! Which update channel a build belongs to, from the version CI gave it.
//!
//! Std only: `build.rs` includes this file to emit `QMT_UPDATE_CHANNEL`, and
//! the app maps the same function's result back with
//! [`super::build_channel_from_version`], so the two cannot disagree.

/// `dev` without a release version (a local, development or Nix build),
/// `nightly` when the version has a `nightly` pre-release
/// (`0.6.1-nightly.20261126120000`), `stable` otherwise.
pub fn name_for_version(version: Option<&str>) -> &'static str {
    let Some(version) = version.map(str::trim).filter(|v| !v.is_empty()) else {
        return "dev";
    };
    let version = version.strip_prefix('v').unwrap_or(version);
    // Build metadata (`+sha`) is not part of the pre-release.
    let version = version.split('+').next().unwrap_or(version);
    let first_identifier = version
        .split_once('-')
        .and_then(|(_, pre)| pre.split('.').next());
    match first_identifier {
        Some("nightly") => "nightly",
        _ => "stable",
    }
}

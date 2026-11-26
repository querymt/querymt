//! Self-update of QueryMT Desktop.
//!
//! Every piece of update policy is a pure function with unit tests: which
//! channel a build follows ([`selected_channel`]), whether a verified release is
//! worth offering ([`evaluate`], [`is_suppressed`]), whether a manifest can be
//! trusted ([`verify`]), how the app was installed ([`install_kind`]) and when
//! to check ([`schedule`]). The store and the UI that wire them to the network
//! and the file system come after this module.
//!
//! Only the preference types are in the web build; the rest is native only.

mod prefs;

#[allow(unused_imports)] // `Channel` is used by the native update logic only
pub use prefs::{Channel, UpdatePrefs};

// `build.rs` includes this file by path; the app only checks it in tests.
#[cfg(all(test, not(target_family = "wasm")))]
mod channel;
#[cfg(not(target_family = "wasm"))]
pub mod check;
#[cfg(not(target_family = "wasm"))]
pub mod fetch;
#[cfg(not(target_family = "wasm"))]
pub mod install;
#[cfg(not(target_family = "wasm"))]
pub mod install_kind;
#[cfg(not(target_family = "wasm"))]
pub mod schedule;
#[cfg(all(test, not(target_family = "wasm")))]
pub(crate) mod test_stub;
#[cfg(not(target_family = "wasm"))]
pub mod verify;

#[cfg(not(target_family = "wasm"))]
pub use native::*;

#[cfg(not(target_family = "wasm"))]
mod native {
    #[cfg(test)]
    use super::channel;
    use super::{Channel, verify::Verified};
    use semver::Version;

    /// The channel a build was made for.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum BuildChannel {
        /// No release version: a local, development or Nix build. It makes no
        /// update request.
        Dev,
        Stable,
        Nightly,
    }

    /// The channel of a build made with `version` (`QMT_VERSION`) as its
    /// release version, or none. `build.rs` runs the same
    /// `channel::name_for_version`; this maps its answer the way
    /// [`build_channel`] does, so tests can check the two together.
    #[cfg(test)]
    pub fn build_channel_from_version(version: Option<&str>) -> BuildChannel {
        match channel::name_for_version(version) {
            "nightly" => BuildChannel::Nightly,
            "stable" => BuildChannel::Stable,
            _ => BuildChannel::Dev,
        }
    }

    /// The channel this build was made for (`build.rs` set `QMT_UPDATE_CHANNEL`).
    pub fn build_channel() -> BuildChannel {
        match env!("QMT_UPDATE_CHANNEL") {
            "nightly" => BuildChannel::Nightly,
            "stable" => BuildChannel::Stable,
            _ => BuildChannel::Dev,
        }
    }

    /// The channel to check: the user's choice, else the build's. `None` for a
    /// development build, which never checks, whatever was saved.
    pub fn selected_channel(saved: Option<Channel>, build: BuildChannel) -> Option<Channel> {
        match build {
            BuildChannel::Dev => None,
            BuildChannel::Stable => Some(saved.unwrap_or(Channel::Stable)),
            BuildChannel::Nightly => Some(saved.unwrap_or(Channel::Nightly)),
        }
    }

    /// The platform key the release manifest uses for this build, if it has
    /// self-updatable installers.
    pub fn platform_key() -> Option<&'static str> {
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            Some("darwin-aarch64")
        } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            Some("darwin-x86_64")
        } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
            Some("windows-x86_64")
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Some("linux-x86_64")
        } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            Some("linux-aarch64")
        } else {
            None
        }
    }

    /// What a check found.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Verdict {
        /// Nothing newer than the running version.
        UpToDate,
        /// A strictly newer, verified version.
        Available { version: String },
    }

    /// Whether a version could not be compared.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct VersionError(pub String);

    impl std::fmt::Display for VersionError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }

    fn parse_version(text: &str) -> Result<Version, VersionError> {
        let text = text.trim();
        Version::parse(text.strip_prefix('v').unwrap_or(text))
            .map_err(|err| VersionError(format!("`{text}` is not a version: {err}")))
    }

    /// Offer the release only when its version is strictly greater than the
    /// running one under semver precedence. A nightly (`X.Y.Z-nightly.<ts>`)
    /// sorts above the stable it follows and below the next stable, so
    /// switching from nightly to an older stable offers nothing. `release` has
    /// passed [`super::verify::verify_manifest`], which also checked its channel.
    pub fn evaluate(running: &str, release: &Verified) -> Result<Verdict, VersionError> {
        let running = parse_version(running)?;
        let offered = parse_version(&release.version)?;
        Ok(if offered > running {
            Verdict::Available {
                version: release.version.clone(),
            }
        } else {
            Verdict::UpToDate
        })
    }

    /// Whether an offered version stays unannounced: automatic checks do not
    /// announce the version the user skipped; a manual check shows everything.
    pub fn is_suppressed(manual: bool, skipped: Option<&str>, offered: &str) -> bool {
        !manual && skipped == Some(offered)
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::verify::{DesktopAsset, Verified};
    use super::*;
    use std::collections::BTreeMap;

    fn release(version: &str, channel: Channel) -> Verified {
        Verified {
            version: version.into(),
            channel,
            commit: "0".repeat(40),
            desktop_updates: BTreeMap::<String, DesktopAsset>::new(),
        }
    }

    #[test]
    fn nightly_build_follows_the_nightly_channel() {
        let build = build_channel_from_version(Some("0.6.1-nightly.20261126120000"));
        assert_eq!(build, BuildChannel::Nightly);
        assert_eq!(selected_channel(None, build), Some(Channel::Nightly));
        // The user's choice wins over the build's channel.
        assert_eq!(
            selected_channel(Some(Channel::Stable), build),
            Some(Channel::Stable)
        );
    }

    #[test]
    fn build_channel_from_version_table() {
        for (version, expected) in [
            (None, BuildChannel::Dev),
            (Some(""), BuildChannel::Dev),
            (Some("   "), BuildChannel::Dev),
            (Some("0.6.0"), BuildChannel::Stable),
            (Some("v0.6.0"), BuildChannel::Stable),
            (Some("0.6.1-nightly.20261126120000"), BuildChannel::Nightly),
            (Some("v0.6.1-nightly.20261126120000"), BuildChannel::Nightly),
            (Some("0.7.0-nightly.1+abc"), BuildChannel::Nightly),
            // Another pre-release is not the nightly channel; nor is a
            // version that only mentions nightly in its build metadata.
            (Some("0.7.0-rc.1"), BuildChannel::Stable),
            (Some("0.7.0+nightly.1"), BuildChannel::Stable),
        ] {
            assert_eq!(build_channel_from_version(version), expected, "{version:?}");
        }
    }

    #[test]
    fn development_build_makes_no_update_request() {
        // The channel half: no channel, even when one was saved.
        assert_eq!(selected_channel(None, BuildChannel::Dev), None);
        assert_eq!(
            selected_channel(Some(Channel::Nightly), BuildChannel::Dev),
            None
        );
        // This test binary is not a release build.
        assert_eq!(build_channel(), BuildChannel::Dev);
    }

    #[test]
    fn newer_signed_version_is_offered() {
        for (running, offered) in [
            ("0.6.0", "0.6.1"),
            ("0.6.0", "0.7.0-nightly.20261126120000"),
            (
                "0.6.1-nightly.20261125000000",
                "0.6.1-nightly.20261126000000",
            ),
            ("0.6.1-nightly.20261126000000", "0.6.1"),
            ("v0.6.0", "0.6.1"),
        ] {
            assert_eq!(
                evaluate(running, &release(offered, Channel::Stable)),
                Ok(Verdict::Available {
                    version: offered.into()
                }),
                "{running} -> {offered}"
            );
        }
    }

    #[test]
    fn same_or_older_version_is_not_offered() {
        for (running, offered) in [
            ("0.6.1", "0.6.1"),
            ("0.6.1", "0.6.0"),
            (
                "0.6.1-nightly.20261126000000",
                "0.6.1-nightly.20261126000000",
            ),
            (
                "0.6.1-nightly.20261126000000",
                "0.6.1-nightly.20261125000000",
            ),
            ("0.6.1", "0.6.1-nightly.20261126000000"),
        ] {
            assert_eq!(
                evaluate(running, &release(offered, Channel::Stable)),
                Ok(Verdict::UpToDate),
                "{running} vs {offered}"
            );
        }
    }

    #[test]
    fn switching_to_stable_never_downgrades() {
        // Running a nightly after 0.6.0; the latest stable is still 0.6.0.
        let running = "0.6.1-nightly.20261126120000";
        assert_eq!(
            evaluate(running, &release("0.6.0", Channel::Stable)),
            Ok(Verdict::UpToDate)
        );
        // Until a stable newer than the running version is published.
        assert_eq!(
            evaluate(running, &release("0.6.1", Channel::Stable)),
            Ok(Verdict::Available {
                version: "0.6.1".into()
            })
        );
    }

    #[test]
    fn a_version_that_is_not_semver_is_an_error() {
        assert!(evaluate("0.6.0", &release("latest", Channel::Stable)).is_err());
        assert!(evaluate("dev", &release("0.6.1", Channel::Stable)).is_err());
    }

    #[test]
    fn skipped_version_stays_quiet() {
        // The user skipped 0.6.1; an automatic check finds it again.
        assert!(is_suppressed(false, Some("0.6.1"), "0.6.1"));
        // A newer version is announced normally, and so is any version when
        // nothing was skipped.
        assert!(!is_suppressed(false, Some("0.6.1"), "0.6.2"));
        assert!(!is_suppressed(false, None, "0.6.1"));
    }

    #[test]
    fn manual_check_shows_skipped_version() {
        assert!(!is_suppressed(true, Some("0.6.1"), "0.6.1"));
        assert!(!is_suppressed(true, None, "0.6.1"));
    }

    #[test]
    fn older_state_loads_with_update_defaults() {
        // Written before update preferences existed.
        let prefs: crate::store::Prefs =
            serde_json::from_str(r#"{"dark":true,"primary_agent":"a"}"#).unwrap();
        assert!(prefs.updates.auto_check);
        assert_eq!(prefs.updates, UpdatePrefs::default());
        // With the build's channel.
        assert_eq!(
            selected_channel(prefs.updates.channel, BuildChannel::Stable),
            Some(Channel::Stable)
        );
        assert_eq!(
            selected_channel(prefs.updates.channel, BuildChannel::Nightly),
            Some(Channel::Nightly)
        );
        // Update preferences survive a save and load.
        let saved = UpdatePrefs {
            channel: Some(Channel::Nightly),
            auto_check: false,
            skipped_version: Some("0.6.1".into()),
            last_check: Some(1_790_000_000),
        };
        let text = serde_json::to_string(&saved).unwrap();
        assert!(text.contains(r#""channel":"nightly""#), "{text}");
        assert_eq!(serde_json::from_str::<UpdatePrefs>(&text).unwrap(), saved);
        // A partial object takes the other defaults.
        let partial: UpdatePrefs = serde_json::from_str(r#"{"skipped_version":"0.6.1"}"#).unwrap();
        assert!(partial.auto_check);
        assert_eq!(partial.skipped_version.as_deref(), Some("0.6.1"));
    }
}

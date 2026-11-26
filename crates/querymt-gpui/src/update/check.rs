//! One update check: fetch, verify, compare, and what the app shows for it.
//!
//! [`run_check`] blocks and belongs on a background thread. What a result
//! means for the user is a pure function, [`after_check`], so the quiet failure
//! of an automatic check and the shown failure of a manual one are tested
//! without a window.

use super::fetch::{self, Feed, FetchError, Progress};
use super::verify::{self, DesktopAsset, Verified, VerifyError};
use super::{Channel, Verdict, VersionError, evaluate, is_suppressed};

/// Why a check found nothing usable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckError {
    Fetch(FetchError),
    Verify(VerifyError),
    Version(VersionError),
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckError::Fetch(err) => err.fmt(f),
            CheckError::Verify(err) => err.fmt(f),
            CheckError::Version(err) => err.fmt(f),
        }
    }
}

/// What a check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckResult {
    UpToDate,
    /// A verified, newer release.
    Available(Verified),
    /// A verified, newer release the user skipped, found by an automatic check.
    Skipped(Verified),
    Failed(CheckError),
}

/// Fetch the manifest of `channel`, verify it with `public_key`, and compare it
/// with the `running` version. `skipped` is the version the user skipped;
/// `manual` says whether the user started the check.
pub fn run_check(
    feed: &Feed,
    public_key: &str,
    channel: Channel,
    running: &str,
    skipped: Option<&str>,
    manual: bool,
) -> CheckResult {
    let (manifest, signature) = match fetch::fetch_manifest(feed, channel) {
        Ok(fetched) => fetched,
        Err(err) => return CheckResult::Failed(CheckError::Fetch(err)),
    };
    let release = match verify::verify_manifest(public_key, &manifest, &signature, channel) {
        Ok(release) => release,
        Err(err) => return CheckResult::Failed(CheckError::Verify(err)),
    };
    match evaluate(running, &release) {
        Err(err) => CheckResult::Failed(CheckError::Version(err)),
        Ok(Verdict::UpToDate) => CheckResult::UpToDate,
        Ok(Verdict::Available { version }) => {
            if is_suppressed(manual, skipped, &version) {
                CheckResult::Skipped(release)
            } else {
                CheckResult::Available(release)
            }
        }
    }
}

/// A newer release the app can offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub version: String,
    pub channel: Channel,
    /// The installer for this platform; `None` when the release has none.
    pub asset: Option<DesktopAsset>,
    pub release_page: String,
}

/// What Settings and the sidebar show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateState {
    Idle,
    UpToDate,
    Available(Offer),
    Downloading(Progress),
    Installing,
    /// Installed (macOS, AppImage); the new version starts on restart.
    ReadyToRestart {
        version: String,
    },
    /// Shown to the user, with how to try again where it helps.
    Failed(String),
}

/// What a finished check changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transition {
    /// The new state, or `None` to leave the shown state alone.
    pub state: Option<UpdateState>,
    /// A line for the log.
    pub log: Option<String>,
}

/// The state a check result leads to. A failed automatic check changes nothing
/// the user sees and is only logged; a failed manual check shows why.
pub fn after_check(
    result: &CheckResult,
    manual: bool,
    platform: Option<&str>,
    feed: &Feed,
) -> Transition {
    match result {
        CheckResult::UpToDate | CheckResult::Skipped(_) => Transition {
            state: Some(UpdateState::UpToDate),
            log: None,
        },
        CheckResult::Available(release) => Transition {
            state: Some(UpdateState::Available(Offer {
                version: release.version.clone(),
                channel: release.channel,
                asset: platform.and_then(|key| release.asset(key)).cloned(),
                release_page: feed.release_page(release.channel, &release.version),
            })),
            log: None,
        },
        CheckResult::Failed(err) => Transition {
            state: manual.then(|| UpdateState::Failed(err.to_string())),
            log: Some(format!(
                "update check failed ({}): {err}",
                if manual { "manual" } else { "automatic" }
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_stub::Stub;
    use super::super::verify::test_signing::TestKey;
    use super::*;
    use sha2::{Digest, Sha256};

    fn manifest(version: &str, channel: &str, file: &str, bytes: &[u8]) -> Vec<u8> {
        serde_json::json!({
            "version": version,
            "channel": channel,
            "commit": "0".repeat(40),
            "desktop_updates": { "linux-x86_64": {
                "file": file,
                "size": bytes.len(),
                "sha256": format!("{:x}", Sha256::digest(bytes)),
            }}
        })
        .to_string()
        .into_bytes()
    }

    /// Serve a signed manifest for `channel` at the stub's feed paths.
    fn publish(stub: &Stub, key: &TestKey, channel: Channel, version: &str, bytes: &[u8]) {
        let feed = Feed::new(&stub.base());
        let file = "QueryMT-Desktop-nightly-linux-x86_64.AppImage";
        let manifest = manifest(version, channel.as_str(), file, bytes);
        let signature = key.sign(
            &manifest,
            &format!("version:{version} channel:{}", channel.as_str()),
        );
        let path = |url: String| url.trim_start_matches(&stub.base()).to_owned();
        stub.set(&path(feed.manifest_url(channel)), manifest);
        stub.set(&path(feed.signature_url(channel)), signature);
    }

    /// A feed whose port nothing listens on.
    fn unreachable_feed() -> Feed {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        Feed::new(&format!("http://127.0.0.1:{port}"))
    }

    fn check(feed: &Feed, key: &TestKey, manual: bool) -> CheckResult {
        run_check(
            feed,
            &key.public(),
            Channel::Nightly,
            "0.6.1-nightly.20261125000000",
            None,
            manual,
        )
    }

    #[test]
    fn automatic_check_offline() {
        let key = TestKey::new(7);
        let feed = unreachable_feed();
        let result = check(&feed, &key, false);
        assert!(
            matches!(
                result,
                CheckResult::Failed(CheckError::Fetch(FetchError::Network(_)))
            ),
            "{result:?}"
        );
        let transition = after_check(&result, false, Some("linux-x86_64"), &feed);
        // No error is shown, and the failure is logged.
        assert_eq!(transition.state, None);
        let log = transition.log.expect("logged");
        assert!(
            log.contains("automatic") && log.contains("Could not reach"),
            "{log}"
        );
    }

    #[test]
    fn manual_check_offline() {
        let key = TestKey::new(7);
        let feed = unreachable_feed();
        let result = check(&feed, &key, true);
        let transition = after_check(&result, true, Some("linux-x86_64"), &feed);
        // Settings shows that the check failed and why.
        match transition.state {
            Some(UpdateState::Failed(why)) => assert!(why.contains("Could not reach"), "{why}"),
            other => panic!("expected a shown failure, got {other:?}"),
        }
        assert!(transition.log.is_some());
    }

    #[test]
    fn a_missing_manifest_is_a_quiet_failure_too() {
        let stub = Stub::start();
        let feed = Feed::new(&stub.base());
        let result = check(&feed, &TestKey::new(7), false);
        assert!(
            matches!(
                result,
                CheckResult::Failed(CheckError::Fetch(FetchError::Status { code: 404, .. }))
            ),
            "{result:?}"
        );
        assert_eq!(after_check(&result, false, None, &feed).state, None);
    }

    #[test]
    fn a_check_requests_only_the_manifest_and_signature_of_the_channel() {
        let stub = Stub::start();
        let key = TestKey::new(7);
        publish(
            &stub,
            &key,
            Channel::Nightly,
            "0.6.1-nightly.20261126000000",
            b"installer",
        );
        let feed = Feed::new(&stub.base());
        let result = check(&feed, &key, false);
        let transition = after_check(&result, false, Some("linux-x86_64"), &feed);
        match transition.state {
            Some(UpdateState::Available(offer)) => {
                assert_eq!(offer.version, "0.6.1-nightly.20261126000000");
                assert_eq!(offer.asset.unwrap().size, 9);
                assert!(
                    offer.release_page.ends_with("/tag/nightly"),
                    "{}",
                    offer.release_page
                );
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            stub.hits(),
            [
                "/download/nightly/release.json",
                "/download/nightly/release.json.minisig"
            ]
        );
    }

    #[test]
    fn a_skipped_version_is_quiet_for_automatic_checks_only() {
        let stub = Stub::start();
        let key = TestKey::new(7);
        let version = "0.6.1-nightly.20261126000000";
        publish(&stub, &key, Channel::Nightly, version, b"installer");
        let feed = Feed::new(&stub.base());
        let run = |manual| {
            run_check(
                &feed,
                &key.public(),
                Channel::Nightly,
                "0.6.1-nightly.20261125000000",
                Some(version),
                manual,
            )
        };
        let automatic = run(false);
        assert!(
            matches!(automatic, CheckResult::Skipped(_)),
            "{automatic:?}"
        );
        assert_eq!(
            after_check(&automatic, false, None, &feed).state,
            Some(UpdateState::UpToDate)
        );
        assert!(matches!(run(true), CheckResult::Available(_)));
    }

    #[test]
    fn a_bad_signature_offers_nothing_and_a_manual_check_says_so() {
        let stub = Stub::start();
        publish(
            &stub,
            &TestKey::new(9),
            Channel::Nightly,
            "0.7.0",
            b"installer",
        );
        let feed = Feed::new(&stub.base());
        // The app trusts another key.
        let result = check(&feed, &TestKey::new(7), true);
        assert!(
            matches!(result, CheckResult::Failed(CheckError::Verify(_))),
            "{result:?}"
        );
        match after_check(&result, true, None, &feed).state {
            Some(UpdateState::Failed(why)) => {
                assert!(why.contains("could not be verified"), "{why}")
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(after_check(&result, false, None, &feed).state, None);
    }
}

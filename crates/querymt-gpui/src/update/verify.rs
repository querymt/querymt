//! Trust: what makes a release manifest and an installer acceptable.
//!
//! A manifest (`release.json`) is accepted only when
//! 1. its minisign signature verifies, over the exact downloaded bytes, with the
//!    public key built into the app;
//! 2. the signature's trusted comment (`version:<v> channel:<c>`) names the
//!    version and channel the manifest itself claims; and
//! 3. that channel is the one the user selected.
//!
//! Only the resulting [`Verified`] value is passed on. An installer is accepted
//! only when its size and SHA-256 equal the entry of the verified manifest.

use std::collections::BTreeMap;

use minisign_verify::{PublicKey, Signature};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::Channel;

/// The update public key, committed with the app. Production code passes it
/// to [`verify_manifest`]; tests pass their own key.
pub const EMBEDDED_PUBLIC_KEY: &str = include_str!("../../packaging/update-key.pub");

/// One installer a release publishes for a platform.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct DesktopAsset {
    pub file: String,
    pub size: u64,
    pub sha256: String,
}

/// A manifest that passed [`verify_manifest`].
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Verified {
    pub version: String,
    pub channel: Channel,
    pub commit: String,
    /// Platform key (`darwin-aarch64`, `windows-x86_64`, …) to its installer.
    #[serde(default)]
    pub desktop_updates: BTreeMap<String, DesktopAsset>,
}

impl Verified {
    /// The installer for a platform key, if the release has one.
    pub fn asset(&self, platform: &str) -> Option<&DesktopAsset> {
        self.desktop_updates.get(platform)
    }
}

/// Why a manifest or an installer was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// The built-in key could not be read (a build defect).
    InvalidKey(String),
    /// The signature file is not a minisign signature.
    MalformedSignature(String),
    /// The signature does not verify with the built-in key.
    BadSignature(String),
    /// The signed bytes are not a manifest.
    MalformedManifest(String),
    /// The trusted comment is not `version:<v> channel:<c>` or names another
    /// version or channel than the manifest.
    CommentMismatch { comment: String },
    /// The manifest is for a channel the user did not select.
    WrongChannel {
        manifest: Channel,
        selected: Channel,
    },
    /// The installer has another size than the manifest says.
    SizeMismatch { expected: u64, actual: u64 },
    /// The installer has another SHA-256 than the manifest says.
    DigestMismatch { expected: String, actual: String },
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::InvalidKey(why) => write!(f, "The built-in update key is invalid: {why}"),
            VerifyError::MalformedSignature(why) => {
                write!(
                    f,
                    "The release could not be verified: its signature is malformed ({why})."
                )
            }
            VerifyError::BadSignature(why) => {
                write!(
                    f,
                    "The release could not be verified: its signature is not valid ({why})."
                )
            }
            VerifyError::MalformedManifest(why) => {
                write!(
                    f,
                    "The release could not be verified: its manifest is malformed ({why})."
                )
            }
            VerifyError::CommentMismatch { comment } => write!(
                f,
                "The release could not be verified: its signature is for something else ({comment:?})."
            ),
            VerifyError::WrongChannel { manifest, selected } => write!(
                f,
                "The release is for the {} channel, not the selected {} channel.",
                manifest.as_str(),
                selected.as_str()
            ),
            VerifyError::SizeMismatch { expected, actual } => write!(
                f,
                "The download does not match the release: {actual} bytes, expected {expected}."
            ),
            VerifyError::DigestMismatch { expected, actual } => write!(
                f,
                "The download does not match the release: SHA-256 {actual}, expected {expected}."
            ),
        }
    }
}

impl std::error::Error for VerifyError {}

/// Verify `manifest` (the exact bytes of `release.json`) with `signature` (the
/// text of `release.json.minisig`) against `public_key` (the text of the
/// `.pub` file), for the `selected` channel.
pub fn verify_manifest(
    public_key: &str,
    manifest: &[u8],
    signature: &str,
    selected: Channel,
) -> Result<Verified, VerifyError> {
    let key =
        PublicKey::decode(public_key).map_err(|err| VerifyError::InvalidKey(err.to_string()))?;
    let signature = Signature::decode(signature)
        .map_err(|err| VerifyError::MalformedSignature(err.to_string()))?;
    // Prehashed signatures only, which is what `minisign -S` writes.
    key.verify(manifest, &signature, false)
        .map_err(|err| VerifyError::BadSignature(err.to_string()))?;

    // Nothing in the manifest is read before its signature verified.
    let release: Verified = serde_json::from_slice(manifest)
        .map_err(|err| VerifyError::MalformedManifest(err.to_string()))?;
    check_comment(signature.trusted_comment(), &release)?;
    check_channel(release.channel, selected)?;
    Ok(release)
}

/// The trusted comment must be exactly `version:<v> channel:<c>` for the
/// manifest's own version and channel, so a signature cannot be moved to
/// another release.
fn check_comment(comment: &str, release: &Verified) -> Result<(), VerifyError> {
    let expected = format!(
        "version:{} channel:{}",
        release.version,
        release.channel.as_str()
    );
    if comment == expected {
        Ok(())
    } else {
        Err(VerifyError::CommentMismatch {
            comment: comment.to_owned(),
        })
    }
}

/// The manifest's channel must be the selected one.
pub fn check_channel(manifest: Channel, selected: Channel) -> Result<(), VerifyError> {
    if manifest == selected {
        Ok(())
    } else {
        Err(VerifyError::WrongChannel { manifest, selected })
    }
}

/// Checks an installer's size and SHA-256 as it is read, so a download of
/// hundreds of megabytes is never held in memory.
pub struct AssetCheck {
    expected: DesktopAsset,
    hasher: Sha256,
    size: u64,
}

impl AssetCheck {
    pub fn new(expected: &DesktopAsset) -> Self {
        Self {
            expected: expected.clone(),
            hasher: Sha256::new(),
            size: 0,
        }
    }

    pub fn update(&mut self, chunk: &[u8]) {
        self.hasher.update(chunk);
        self.size += chunk.len() as u64;
    }

    /// Whether what was read is the asset the verified manifest names.
    pub fn finish(self) -> Result<(), VerifyError> {
        if self.size != self.expected.size {
            return Err(VerifyError::SizeMismatch {
                expected: self.expected.size,
                actual: self.size,
            });
        }
        let actual = format!("{:x}", self.hasher.finalize());
        if !actual.eq_ignore_ascii_case(&self.expected.sha256) {
            return Err(VerifyError::DigestMismatch {
                expected: self.expected.sha256,
                actual,
            });
        }
        Ok(())
    }
}

/// [`AssetCheck`] over bytes already in memory.
#[cfg(test)]
pub fn verify_asset(expected: &DesktopAsset, bytes: &[u8]) -> Result<(), VerifyError> {
    let mut check = AssetCheck::new(expected);
    check.update(bytes);
    check.finish()
}

#[cfg(test)]
pub(crate) mod test_signing {
    //! Signs the way `minisign -S` does, with a throwaway key.

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use blake2::{Blake2b512, Digest as _};
    use ed25519_dalek::{Signer as _, SigningKey};

    pub struct TestKey {
        signing: SigningKey,
        key_id: [u8; 8],
    }

    impl TestKey {
        /// A key from a fixed seed: tests need no randomness.
        pub fn new(seed: u8) -> Self {
            Self {
                signing: SigningKey::from_bytes(&[seed; 32]),
                key_id: [seed; 8],
            }
        }

        /// A key whose id is not the one its seed would give (to forge a
        /// signature that claims another key's id).
        pub fn with_key_id(seed: u8, key_id: u8) -> Self {
            Self {
                signing: SigningKey::from_bytes(&[seed; 32]),
                key_id: [key_id; 8],
            }
        }

        /// The text of the `.pub` file.
        pub fn public(&self) -> String {
            let mut bin = b"Ed".to_vec();
            bin.extend_from_slice(&self.key_id);
            bin.extend_from_slice(self.signing.verifying_key().as_bytes());
            format!(
                "untrusted comment: minisign public key\n{}\n",
                STANDARD.encode(bin)
            )
        }

        /// The text of the `.minisig` file for `message`.
        pub fn sign(&self, message: &[u8], trusted_comment: &str) -> String {
            let prehashed = Blake2b512::digest(message);
            let signature = self.signing.sign(&prehashed).to_bytes();
            let mut bin = b"ED".to_vec();
            bin.extend_from_slice(&self.key_id);
            bin.extend_from_slice(&signature);
            let mut global = signature.to_vec();
            global.extend_from_slice(trusted_comment.as_bytes());
            let global = self.signing.sign(&global).to_bytes();
            format!(
                "untrusted comment: signature from minisign secret key\n{}\ntrusted comment: {}\n{}\n",
                STANDARD.encode(bin),
                trusted_comment,
                STANDARD.encode(global)
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_signing::TestKey;
    use super::*;

    fn manifest(version: &str, channel: &str) -> Vec<u8> {
        serde_json::json!({
            "version": version,
            "channel": channel,
            "commit": "0123456789abcdef0123456789abcdef01234567",
            "published_at": "2026-11-26T12:00:00Z",
            "desktop_updates": {
                "windows-x86_64": {
                    "file": "QueryMT-Desktop-nightly-windows-x64-setup.exe",
                    "size": 5,
                    "sha256": hex_sha256(b"hello"),
                }
            }
        })
        .to_string()
        .into_bytes()
    }

    fn hex_sha256(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn signed(key: &TestKey, version: &str, channel: &str) -> (Vec<u8>, String) {
        let manifest = manifest(version, channel);
        let signature = key.sign(&manifest, &format!("version:{version} channel:{channel}"));
        (manifest, signature)
    }

    #[test]
    fn embedded_key_is_a_valid_minisign_key() {
        PublicKey::decode(EMBEDDED_PUBLIC_KEY).expect("the committed update key decodes");
    }

    #[test]
    fn a_correctly_signed_manifest_is_accepted() {
        let key = TestKey::new(7);
        let (manifest, signature) = signed(&key, "0.6.1-nightly.20261126120000", "nightly");
        let verified =
            verify_manifest(&key.public(), &manifest, &signature, Channel::Nightly).unwrap();
        assert_eq!(verified.version, "0.6.1-nightly.20261126120000");
        assert_eq!(verified.channel, Channel::Nightly);
        let asset = verified.asset("windows-x86_64").unwrap();
        assert_eq!(asset.size, 5);
        assert!(verified.asset("linux-x86_64").is_none());
    }

    #[test]
    fn bad_signature_is_refused() {
        let key = TestKey::new(7);
        let (manifest, signature) = signed(&key, "0.6.1", "stable");

        // The manifest changed after signing.
        let tampered = String::from_utf8(manifest.clone())
            .unwrap()
            .replace("0.6.1", "9.9.9")
            .into_bytes();
        let err =
            verify_manifest(&key.public(), &tampered, &signature, Channel::Stable).unwrap_err();
        assert!(matches!(err, VerifyError::BadSignature(_)), "{err:?}");

        // Signed by another key that claims the real key's id: the id matches,
        // the signature itself does not verify.
        let other = TestKey::new(8);
        let forged = TestKey::with_key_id(8, 7).sign(&manifest, "version:0.6.1 channel:stable");
        let err = verify_manifest(&key.public(), &manifest, &forged, Channel::Stable).unwrap_err();
        assert!(matches!(err, VerifyError::BadSignature(_)), "{err:?}");

        // Signed with a key the app does not have.
        let err = verify_manifest(
            &key.public(),
            &manifest,
            &other.sign(&manifest, "version:0.6.1 channel:stable"),
            Channel::Stable,
        )
        .unwrap_err();
        assert!(matches!(err, VerifyError::BadSignature(_)), "{err:?}");

        // Not a signature at all.
        let err = verify_manifest(&key.public(), &manifest, "nope", Channel::Stable).unwrap_err();
        assert!(matches!(err, VerifyError::MalformedSignature(_)), "{err:?}");

        // A manual check says the release could not be verified.
        assert!(
            VerifyError::BadSignature("x".into())
                .to_string()
                .contains("could not be verified")
        );
    }

    #[test]
    fn the_committed_key_refuses_a_signature_from_another_key() {
        // What an attacker without the update key can produce.
        let attacker = TestKey::new(9);
        let (manifest, signature) = signed(&attacker, "0.6.1", "stable");
        let err = verify_manifest(EMBEDDED_PUBLIC_KEY, &manifest, &signature, Channel::Stable)
            .unwrap_err();
        assert!(!matches!(err, VerifyError::InvalidKey(_)), "{err:?}");
        assert!(err.to_string().contains("could not be verified"), "{err}");
    }

    #[test]
    fn signature_for_another_version_is_refused() {
        let key = TestKey::new(7);
        let manifest = manifest("0.6.1", "stable");
        // A genuine signature of this manifest, from a release of another
        // version or channel (replayed with its own trusted comment).
        for comment in [
            "version:0.6.0 channel:stable",
            "version:0.6.1 channel:nightly",
            "version:0.6.1",
            "version:0.6.1 channel:stable extra",
            "",
        ] {
            let signature = key.sign(&manifest, comment);
            let err =
                verify_manifest(&key.public(), &manifest, &signature, Channel::Stable).unwrap_err();
            assert!(
                matches!(err, VerifyError::CommentMismatch { .. }),
                "{comment:?}: {err:?}"
            );
        }
    }

    #[test]
    fn a_manifest_for_another_channel_is_refused() {
        let key = TestKey::new(7);
        let (manifest, signature) = signed(&key, "0.6.1", "stable");
        let err =
            verify_manifest(&key.public(), &manifest, &signature, Channel::Nightly).unwrap_err();
        assert_eq!(
            err,
            VerifyError::WrongChannel {
                manifest: Channel::Stable,
                selected: Channel::Nightly
            }
        );
        assert!(check_channel(Channel::Nightly, Channel::Nightly).is_ok());
    }

    #[test]
    fn a_signed_manifest_that_is_not_json_is_refused() {
        let key = TestKey::new(7);
        let body = b"<html>not json</html>";
        let signature = key.sign(body, "version:1 channel:stable");
        let err = verify_manifest(&key.public(), body, &signature, Channel::Stable).unwrap_err();
        assert!(matches!(err, VerifyError::MalformedManifest(_)), "{err:?}");
    }

    fn asset(bytes: &[u8]) -> DesktopAsset {
        DesktopAsset {
            file: "QueryMT-Desktop-nightly-windows-x64-setup.exe".into(),
            size: bytes.len() as u64,
            sha256: hex_sha256(bytes),
        }
    }

    #[test]
    fn the_listed_asset_is_accepted() {
        assert_eq!(verify_asset(&asset(b"hello"), b"hello"), Ok(()));
        // Streaming in pieces gives the same answer.
        let mut check = AssetCheck::new(&asset(b"hello"));
        check.update(b"he");
        check.update(b"llo");
        assert_eq!(check.finish(), Ok(()));
    }

    #[test]
    fn corrupted_download_is_rejected() {
        // Same length, other content.
        let err = verify_asset(&asset(b"hello"), b"jello").unwrap_err();
        assert!(matches!(err, VerifyError::DigestMismatch { .. }), "{err:?}");
        // Truncated.
        let err = verify_asset(&asset(b"hello"), b"hell").unwrap_err();
        assert_eq!(
            err,
            VerifyError::SizeMismatch {
                expected: 5,
                actual: 4
            }
        );
        // Longer.
        assert!(verify_asset(&asset(b"hello"), b"hello!").is_err());
        let mut check = AssetCheck::new(&asset(b"hello"));
        check.update(b"jel");
        check.update(b"lo");
        assert!(matches!(
            check.finish(),
            Err(VerifyError::DigestMismatch { .. })
        ));
    }

    #[test]
    fn nightly_replaced_during_download() {
        // The manifest is from run N; the asset behind the nightly URL is by
        // now from run N+1 (same name, other content).
        let key = TestKey::new(7);
        let run_n = b"installer of run N";
        let run_n_plus_1 = b"installer of run N+1, a different build";
        let manifest = serde_json::json!({
            "version": "0.6.1-nightly.20261126120000",
            "channel": "nightly",
            "commit": "0123456789abcdef0123456789abcdef01234567",
            "desktop_updates": { "linux-x86_64": {
                "file": "QueryMT-Desktop-nightly-linux-x86_64.AppImage",
                "size": run_n.len(),
                "sha256": hex_sha256(run_n),
            }}
        })
        .to_string()
        .into_bytes();
        let signature = key.sign(
            &manifest,
            "version:0.6.1-nightly.20261126120000 channel:nightly",
        );
        let verified =
            verify_manifest(&key.public(), &manifest, &signature, Channel::Nightly).unwrap();
        let listed = verified.asset("linux-x86_64").unwrap();

        // The same error as any corrupted download (the install fails, and the
        // next check offers the new nightly).
        let err = verify_asset(listed, run_n_plus_1).unwrap_err();
        assert!(
            matches!(
                err,
                VerifyError::SizeMismatch { .. } | VerifyError::DigestMismatch { .. }
            ),
            "{err:?}"
        );
        assert!(
            err.to_string().contains("does not match the release"),
            "{err}"
        );
        assert_eq!(verify_asset(listed, run_n), Ok(()));
    }
}

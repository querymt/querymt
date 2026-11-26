//! The network side of updates: the signed manifest, and installer downloads.
//!
//! Everything here blocks, so it only runs on a background thread. Downloads
//! run on a dedicated `std::thread` (not the executor's pool, which a
//! 15-minute download would hold) and report progress over a channel. The
//! feed's base URL is injectable, so tests serve it from a local socket.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use async_channel::Sender;

use super::Channel;
use super::verify::{AssetCheck, DesktopAsset, VerifyError};

/// Where releases are published.
pub const RELEASES_URL: &str = "https://github.com/querymt/querymt/releases";

/// A manifest is a few kilobytes; anything larger is not one.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_SIGNATURE_BYTES: u64 = 16 * 1024;

/// The release feed: manifest and installer locations under one base URL.
#[derive(Clone, Debug)]
pub struct Feed {
    base: String,
    pub manifest_timeout: Duration,
    pub download_timeout: Duration,
}

impl Default for Feed {
    fn default() -> Self {
        Self::new(RELEASES_URL)
    }
}

impl Feed {
    pub fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            manifest_timeout: Duration::from_secs(30),
            download_timeout: Duration::from_secs(15 * 60),
        }
    }

    /// The manifest of a channel. GitHub's `latest` skips pre-releases, so it
    /// always names the newest stable; the nightly is one rolling release.
    pub fn manifest_url(&self, channel: Channel) -> String {
        match channel {
            Channel::Stable => format!("{}/latest/download/release.json", self.base),
            Channel::Nightly => format!("{}/download/nightly/release.json", self.base),
        }
    }

    pub fn signature_url(&self, channel: Channel) -> String {
        format!("{}.minisig", self.manifest_url(channel))
    }

    /// The release tag of a version: `v<version>` for stable, `nightly`.
    pub fn tag(channel: Channel, version: &str) -> String {
        match channel {
            Channel::Stable => format!("v{}", version.trim_start_matches('v')),
            Channel::Nightly => "nightly".to_owned(),
        }
    }

    pub fn asset_url(&self, channel: Channel, version: &str, file: &str) -> String {
        format!(
            "{}/download/{}/{file}",
            self.base,
            Self::tag(channel, version)
        )
    }

    /// The release page shown when the app cannot install by itself.
    pub fn release_page(&self, channel: Channel, version: &str) -> String {
        format!("{}/tag/{}", self.base, Self::tag(channel, version))
    }
}

/// Why a request failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchError {
    /// No connection, a timeout, a TLS failure or another transport error.
    Network(String),
    /// The server answered with an error status (a missing manifest is 404).
    Status { code: u16, url: String },
    /// The answer is larger than any manifest or installer can be.
    TooLarge { url: String },
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Network(why) => write!(f, "Could not reach the release server: {why}"),
            FetchError::Status { code, url } => {
                write!(f, "The release server answered {code} for {url}.")
            }
            FetchError::TooLarge { url } => write!(f, "The answer from {url} is too large."),
        }
    }
}

impl std::error::Error for FetchError {}

fn agent(feed: &Feed, timeout: Duration) -> ureq::Agent {
    // Proxy settings come from the environment (ureq's default). Plain HTTP is
    // only accepted for a feed that is not the real one, which tests use.
    let config = ureq::Agent::config_builder()
        .https_only(feed.base.starts_with("https://"))
        .max_redirects(5)
        .timeout_global(Some(timeout))
        .timeout_connect(Some(Duration::from_secs(15)))
        .user_agent(concat!("querymt-gpui/", env!("QMT_BUILD_VERSION")))
        .build();
    ureq::Agent::new_with_config(config)
}

fn map_error(err: ureq::Error, url: &str) -> FetchError {
    match err {
        ureq::Error::StatusCode(code) => FetchError::Status {
            code,
            url: url.to_owned(),
        },
        other => FetchError::Network(other.to_string()),
    }
}

fn get_limited(agent: &ureq::Agent, url: &str, limit: u64) -> Result<Vec<u8>, FetchError> {
    let mut response = agent.get(url).call().map_err(|err| map_error(err, url))?;
    let bytes = response
        .body_mut()
        .with_config()
        .limit(limit + 1)
        .read_to_vec()
        .map_err(|err| map_error(err, url))?;
    if bytes.len() as u64 > limit {
        return Err(FetchError::TooLarge {
            url: url.to_owned(),
        });
    }
    Ok(bytes)
}

/// The manifest of a channel and its signature: the exact bytes to verify, and
/// the text of the `.minisig` file. Nothing is interpreted here.
pub fn fetch_manifest(feed: &Feed, channel: Channel) -> Result<(Vec<u8>, String), FetchError> {
    let agent = agent(feed, feed.manifest_timeout);
    let manifest_url = feed.manifest_url(channel);
    let manifest = get_limited(&agent, &manifest_url, MAX_MANIFEST_BYTES)?;
    let signature_url = feed.signature_url(channel);
    let signature = get_limited(&agent, &signature_url, MAX_SIGNATURE_BYTES)?;
    let signature = String::from_utf8(signature)
        .map_err(|_| FetchError::Network(format!("{signature_url} is not text")))?;
    Ok((manifest, signature))
}

/// How far a download is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub received: u64,
    pub total: u64,
}

/// Why a download failed.
#[derive(Debug, PartialEq, Eq)]
pub enum DownloadError {
    Fetch(FetchError),
    /// The size or SHA-256 differs from the verified manifest entry. This is
    /// also what a nightly replaced during the download looks like.
    Verify(VerifyError),
    /// The file could not be written.
    Io(String),
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadError::Fetch(err) => err.fmt(f),
            DownloadError::Verify(err) => err.fmt(f),
            DownloadError::Io(why) => write!(f, "Could not save the download: {why}"),
        }
    }
}

impl std::error::Error for DownloadError {}

/// Send a progress report at most this often, in bytes.
const PROGRESS_STEP: u64 = 64 * 1024;

/// Download `url` to `dest`, checking its size and SHA-256 against `expected`
/// as it arrives. A mismatch, a network failure or a write failure deletes
/// `dest`: a file left at `dest` is a verified installer.
pub fn download_asset(
    feed: &Feed,
    url: &str,
    expected: &DesktopAsset,
    dest: &Path,
    progress: &Sender<Progress>,
) -> Result<(), DownloadError> {
    let result = download_inner(feed, url, expected, dest, progress);
    if result.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    result
}

fn download_inner(
    feed: &Feed,
    url: &str,
    expected: &DesktopAsset,
    dest: &Path,
    progress: &Sender<Progress>,
) -> Result<(), DownloadError> {
    let agent = agent(feed, feed.download_timeout);
    let response = agent
        .get(url)
        .call()
        .map_err(|err| DownloadError::Fetch(map_error(err, url)))?;
    let mut body = response.into_body();
    // One byte more than expected, so a longer body is seen as a size mismatch
    // instead of being read to its end.
    let mut reader = body.as_reader().take(expected.size.saturating_add(1));
    let mut file = File::create(dest).map_err(|err| DownloadError::Io(err.to_string()))?;
    let mut check = AssetCheck::new(expected);
    let mut buffer = vec![0u8; 64 * 1024];
    let mut received = 0u64;
    let mut reported = 0u64;
    let total = expected.size;
    let _ = progress.try_send(Progress { received: 0, total });
    loop {
        let n = reader
            .read(&mut buffer)
            .map_err(|err| DownloadError::Fetch(FetchError::Network(err.to_string())))?;
        if n == 0 {
            break;
        }
        check.update(&buffer[..n]);
        file.write_all(&buffer[..n])
            .map_err(|err| DownloadError::Io(err.to_string()))?;
        received += n as u64;
        if received - reported >= PROGRESS_STEP {
            reported = received;
            let _ = progress.try_send(Progress { received, total });
        }
    }
    file.flush()
        .map_err(|err| DownloadError::Io(err.to_string()))?;
    drop(file);
    check.finish().map_err(DownloadError::Verify)?;
    let _ = progress.try_send(Progress {
        received: total,
        total,
    });
    Ok(())
}

/// [`download_asset`] on its own thread.
pub fn spawn_download(
    feed: Feed,
    url: String,
    expected: DesktopAsset,
    dest: std::path::PathBuf,
    progress: Sender<Progress>,
) -> std::thread::JoinHandle<Result<(), DownloadError>> {
    std::thread::Builder::new()
        .name("update-download".into())
        .spawn(move || download_asset(&feed, &url, &expected, &dest, &progress))
        .expect("spawn the update download thread")
}

#[cfg(test)]
mod tests {
    use super::super::test_stub::Stub;
    use super::*;
    use sha2::{Digest, Sha256};

    fn asset_for(body: &[u8]) -> DesktopAsset {
        DesktopAsset {
            file: "QueryMT-Desktop-nightly-linux-x86_64.AppImage".into(),
            size: body.len() as u64,
            sha256: format!("{:x}", Sha256::digest(body)),
        }
    }

    /// 300 KiB of varied bytes: several progress steps.
    fn big_body() -> Vec<u8> {
        (0..300 * 1024).map(|i| (i % 251) as u8).collect()
    }

    const PATH: &str = "/download/nightly/QueryMT-Desktop-nightly-linux-x86_64.AppImage";

    fn download(
        stub: &Stub,
        expected: &DesktopAsset,
        dest: &Path,
    ) -> (Result<(), DownloadError>, Vec<Progress>) {
        let feed = Feed::new(&stub.base());
        let (tx, rx) = async_channel::unbounded();
        let url = feed.asset_url(Channel::Nightly, "0.6.1-nightly.1", &expected.file);
        let handle = spawn_download(feed, url, expected.clone(), dest.to_path_buf(), tx);
        let result = handle.join().expect("download thread");
        let mut reports = Vec::new();
        while let Ok(report) = rx.try_recv() {
            reports.push(report);
        }
        (result, reports)
    }

    #[test]
    fn download_reports_progress() {
        let body = big_body();
        let stub = Stub::start();
        stub.set(PATH, body.clone());
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("update.bin");
        let (result, reports) = download(&stub, &asset_for(&body), &dest);
        assert_eq!(result, Ok(()));
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        // Starts at zero, ends at the total, never goes backwards, and has
        // steps in between for a progress bar.
        let total = body.len() as u64;
        assert_eq!(reports.first(), Some(&Progress { received: 0, total }));
        assert_eq!(
            reports.last(),
            Some(&Progress {
                received: total,
                total
            })
        );
        assert!(reports.len() >= 4, "{reports:?}");
        assert!(reports.windows(2).all(|w| w[0].received <= w[1].received));
        assert!(reports.iter().all(|r| r.total == total));
    }

    #[test]
    fn a_corrupted_download_leaves_no_file() {
        let body = big_body();
        let mut corrupted = body.clone();
        corrupted[1000] ^= 0xff;
        let stub = Stub::start();
        stub.set(PATH, corrupted);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("update.bin");
        let (result, _) = download(&stub, &asset_for(&body), &dest);
        assert!(
            matches!(
                result,
                Err(DownloadError::Verify(VerifyError::DigestMismatch { .. }))
            ),
            "{result:?}"
        );
        assert!(!dest.exists(), "the download is deleted");
    }

    #[test]
    fn a_longer_or_shorter_download_is_a_size_mismatch() {
        let body = big_body();
        let stub = Stub::start();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("update.bin");
        for served in [
            body[..body.len() - 10].to_vec(),
            [body.clone(), vec![1; 50]].concat(),
        ] {
            stub.set(PATH, served);
            let (result, _) = download(&stub, &asset_for(&body), &dest);
            assert!(
                matches!(
                    result,
                    Err(DownloadError::Verify(VerifyError::SizeMismatch { .. }))
                ),
                "{result:?}"
            );
            assert!(!dest.exists());
        }
    }

    #[test]
    fn a_missing_asset_is_a_fetch_error_and_leaves_no_file() {
        let body = big_body();
        let stub = Stub::start();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("update.bin");
        let (result, _) = download(&stub, &asset_for(&body), &dest);
        assert!(
            matches!(
                result,
                Err(DownloadError::Fetch(FetchError::Status { code: 404, .. }))
            ),
            "{result:?}"
        );
        assert!(!dest.exists());
    }

    #[test]
    fn urls_follow_the_release_layout() {
        let feed = Feed::new("https://example.test/releases/");
        assert_eq!(
            feed.manifest_url(Channel::Stable),
            "https://example.test/releases/latest/download/release.json"
        );
        assert_eq!(
            feed.signature_url(Channel::Nightly),
            "https://example.test/releases/download/nightly/release.json.minisig"
        );
        assert_eq!(
            feed.asset_url(Channel::Stable, "0.6.1", "a.exe"),
            "https://example.test/releases/download/v0.6.1/a.exe"
        );
        assert_eq!(
            feed.asset_url(Channel::Nightly, "0.6.1-nightly.1", "a.exe"),
            "https://example.test/releases/download/nightly/a.exe"
        );
        assert_eq!(
            feed.release_page(Channel::Stable, "0.6.1"),
            "https://example.test/releases/tag/v0.6.1"
        );
        assert_eq!(
            Feed::default().manifest_url(Channel::Stable),
            format!("{RELEASES_URL}/latest/download/release.json")
        );
    }
}

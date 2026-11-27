//! Replace the running `.app` bundle with the one in a downloaded
//! `app.tar.gz` (design D10).
//!
//! The swap itself is the `self_update` crate's: it downloads, extracts next
//! to the installed bundle, stashes the old bundle, renames the new one in,
//! rolls back on failure and removes the stash. What is ours is what it cannot
//! know, supplied as two hooks:
//!
//! - `verify_archive`, before anything is extracted: the size and SHA-256 from
//!   the signed manifest, and a scan that refuses archives that could write
//!   outside the bundle.
//! - `verify_binary`, on the staged bundle before the first rename: the new
//!   bundle is complete and signed, then the local agents are stopped. An error
//!   from a hook leaves the installed bundle untouched.
//!
//! Runs on the calling thread (blocking); call it from a background thread.

use std::io::Read as _;
use std::path::{Component, Path};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_channel::Sender;
use self_update::{Release, ReleaseAsset, ReleaseSource};

use super::InstallError;
use crate::update::fetch::Progress;
use crate::update::verify::{AssetCheck, DesktopAsset};

/// The bundle every update archive carries, and the executable inside it.
const BUNDLE: &str = "QueryMT Desktop.app";
const EXECUTABLE: &str = "Contents/MacOS/querymt-gpui";

/// A step that either passes or says why not.
pub type Step = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;
/// A check of the staged bundle.
pub type BundleCheck = Arc<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

/// What the installer asks of the app. The real ones run `codesign` and stop
/// the local agents; tests supply their own.
#[derive(Clone)]
pub struct Steps {
    /// Verify the signature of the staged bundle.
    pub codesign: BundleCheck,
    /// Stop the local process agents and wait for them. Called once, after the
    /// new bundle passed every check and before the first rename. An error
    /// aborts the install.
    pub stop_agents: Step,
}

impl Steps {
    pub fn real(stop_agents: Step) -> Self {
        Self {
            codesign: Arc::new(codesign),
            stop_agents,
        }
    }
}

fn codesign(bundle: &Path) -> Result<(), String> {
    let out = std::process::Command::new("codesign")
        .args(["--verify", "--strict"])
        .arg(bundle)
        .output()
        .map_err(|err| format!("could not run codesign: {err}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

/// The one release the crate may choose from: the one the signed manifest
/// offered. `update()` is only called after our own version check, so the
/// crate is told it runs `0.0.0` and never decides again.
struct Offered {
    version: String,
    file: String,
    url: String,
}

impl ReleaseSource for Offered {
    fn get_releases(&self) -> self_update::Result<Vec<Release>> {
        Ok(vec![
            Release::builder()
                .version(&self.version)
                .asset(ReleaseAsset::new(&self.file, &self.url))
                .build()?,
        ])
    }
}

/// Download `url` (the asset `asset` of `version`), check it and replace the
/// installed `bundle` with what it carries. On any error the installed bundle
/// is as it was and nothing is left next to it.
pub fn install(
    bundle: &Path,
    version: &str,
    url: &str,
    asset: &DesktopAsset,
    timeout: std::time::Duration,
    progress: Sender<Progress>,
    steps: Steps,
) -> Result<(), InstallError> {
    let last_sent = Arc::new(AtomicU64::new(0));
    let expected = asset.clone();
    let for_archive = asset.clone();
    let stop = steps.stop_agents.clone();
    let codesign = steps.codesign.clone();

    let mut builder = self_update::backends::custom::Update::configure();
    builder
        .source(Offered {
            version: version.to_owned(),
            file: asset.file.clone(),
            url: url.to_owned(),
        })
        .bin_name("querymt-gpui")
        .current_version("0.0.0")
        .target(&asset.file)
        .bundle_path_in_archive(BUNDLE)
        .bundle_install_path(bundle)
        .no_confirm(true)
        .show_output(false)
        .show_download_progress(false)
        .timeout(timeout)
        .progress_callback(move |received, total| {
            // At most one report per 64 KiB, and always the last.
            let total = total.unwrap_or(expected.size);
            let sent = last_sent.load(Ordering::Relaxed);
            if received == total || received.saturating_sub(sent) >= 64 * 1024 {
                last_sent.store(received, Ordering::Relaxed);
                let _ = progress.try_send(Progress { received, total });
            }
        })
        .verify_archive(move |archive| {
            check_archive(archive, &for_archive)
                .map_err(|why| self_update::Error::archive_verification_rejected(why.0))
        })
        .verify_binary(move |staged| {
            check_bundle(staged)
                .and_then(|()| {
                    codesign(staged).map_err(|why| {
                        InstallError(format!(
                            "The downloaded app failed its signature check: {why}"
                        ))
                    })
                })
                .and_then(|()| {
                    stop().map_err(|why| {
                        InstallError(format!("The local agents did not stop: {why}"))
                    })
                })
                .map_err(|why| self_update::Error::verification_rejected(why.0))
        });

    builder
        .build()
        .and_then(|update| update.update())
        .map(|_| ())
        .map_err(explain)
}

/// Words for the Updates card; the crate's own messages name its internals.
fn explain(err: self_update::Error) -> InstallError {
    use self_update::Error as E;
    InstallError(match err {
        E::VerificationRejected { reason, .. } | E::ArchiveVerificationRejected { reason, .. } => {
            reason.unwrap_or_else(|| "The update was refused.".into())
        }
        E::AppTranslocated { .. } => {
            "The app is running from a temporary location. Move it to Applications, open it \
             from there and try again."
                .into()
        }
        E::InstallPathNotWritable { path, .. } => format!(
            "`{}` is not writable. Move the app to a folder you can write to.",
            path.display()
        ),
        other => format!("The update could not be installed: {other}"),
    })
}

/// The downloaded archive is the asset the signed manifest names, and cannot
/// write outside the bundle.
fn check_archive(archive: &Path, asset: &DesktopAsset) -> Result<(), InstallError> {
    let mut file = std::fs::File::open(archive)
        .map_err(|err| InstallError(format!("Could not open the download: {err}")))?;
    let mut check = AssetCheck::new(asset);
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|err| InstallError(format!("Could not read the download: {err}")))?;
        if n == 0 {
            break;
        }
        check.update(&buffer[..n]);
    }
    check
        .finish()
        .map_err(|err| InstallError(err.to_string()))?;
    scan_archive(archive)
}

/// Every entry must be inside `QueryMT Desktop.app` (relative, no `..`), and
/// links must be relative and stay inside. The crate alone would skip such
/// entries without an error and accept links to anywhere.
fn scan_archive(archive: &Path) -> Result<(), InstallError> {
    let file = std::fs::File::open(archive)
        .map_err(|err| InstallError(format!("Could not open the download: {err}")))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let bad =
        |err: std::io::Error| InstallError(format!("The download is not a valid archive: {err}"));
    let mut entries = 0usize;
    for entry in tar.entries().map_err(bad)? {
        let entry = entry.map_err(bad)?;
        let kind = entry.header().entry_type();
        if kind == tar::EntryType::XGlobalHeader {
            continue;
        }
        let path = entry.path().map_err(bad)?.into_owned();
        check_entry_path(&path)?;
        if kind == tar::EntryType::Link {
            return Err(InstallError(format!(
                "The archive has a hard link (`{}`).",
                path.display()
            )));
        }
        if let Some(target) = entry.link_name().map_err(bad)? {
            check_link_target(&path, &target)?;
        }
        entries += 1;
    }
    if entries == 0 {
        return Err(InstallError("The download is an empty archive.".into()));
    }
    Ok(())
}

fn check_entry_path(path: &Path) -> Result<(), InstallError> {
    let mut first = true;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) if first && part == BUNDLE => first = false,
            Component::Normal(_) if !first => {}
            _ => {
                return Err(InstallError(format!(
                    "The archive entry `{}` is not inside `{BUNDLE}`.",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn check_link_target(entry: &Path, target: &Path) -> Result<(), InstallError> {
    let outside = target
        .components()
        .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir));
    if outside {
        return Err(InstallError(format!(
            "The archive link `{}` points outside the bundle.",
            entry.display()
        )));
    }
    Ok(())
}

/// The staged bundle has an `Info.plist` and a runnable main executable. The
/// crate only compares paths when the updater runs from inside the bundle.
fn check_bundle(staged: &Path) -> Result<(), InstallError> {
    if !staged.join("Contents/Info.plist").is_file() {
        return Err(InstallError(
            "The downloaded app has no Contents/Info.plist.".into(),
        ));
    }
    let runnable = {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(staged.join(EXECUTABLE))
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    };
    if !runnable {
        return Err(InstallError(format!(
            "The downloaded app has no runnable {EXECUTABLE}."
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Duration;

    use sha2::{Digest as _, Sha256};

    use super::*;
    use crate::update::test_stub::Stub;

    type Tar = tar::Builder<flate2::write::GzEncoder<Vec<u8>>>;

    fn builder() -> Tar {
        tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ))
    }

    fn finish(tar: Tar) -> Vec<u8> {
        tar.into_inner().unwrap().finish().unwrap()
    }

    fn append(tar: &mut Tar, path: &str, mode: u32, data: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(mode);
        header.set_cksum();
        tar.append_data(&mut header, path, data).unwrap();
    }

    /// A good update archive whose `version.txt` says `version`.
    fn bundle_archive(version: &str, extra: &[u8]) -> Vec<u8> {
        let mut tar = builder();
        append(
            &mut tar,
            &format!("{BUNDLE}/Contents/Info.plist"),
            0o644,
            b"<plist/>",
        );
        append(
            &mut tar,
            &format!("{BUNDLE}/{EXECUTABLE}"),
            0o755,
            b"#!/bin/sh\n",
        );
        append(
            &mut tar,
            &format!("{BUNDLE}/Contents/Resources/version.txt"),
            0o644,
            version.as_bytes(),
        );
        if !extra.is_empty() {
            append(
                &mut tar,
                &format!("{BUNDLE}/Contents/Resources/blob.bin"),
                0o644,
                extra,
            );
        }
        finish(tar)
    }

    /// Bytes gzip cannot shrink, so the download is big enough for several
    /// progress reports.
    fn incompressible(len: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 32) as u8
            })
            .collect()
    }

    fn asset_of(file: &str, bytes: &[u8]) -> DesktopAsset {
        DesktopAsset {
            file: file.into(),
            size: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
        }
    }

    const FILE: &str = "QueryMT-Desktop-nightly-macos-aarch64.app.tar.gz";
    const PATH: &str = "/download/nightly/QueryMT-Desktop-nightly-macos-aarch64.app.tar.gz";

    /// An installed bundle whose `version.txt` says `old`, in a fresh
    /// `Applications` directory.
    fn installed() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let apps = root.path().join("Applications");
        let bundle = apps.join(BUNDLE);
        fs::create_dir_all(bundle.join("Contents/Resources")).unwrap();
        fs::write(bundle.join("Contents/Resources/version.txt"), "old").unwrap();
        (root, bundle)
    }

    fn version(bundle: &Path) -> String {
        fs::read_to_string(bundle.join("Contents/Resources/version.txt")).unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn ok_steps() -> Steps {
        Steps {
            codesign: Arc::new(|_| Ok(())),
            stop_agents: Arc::new(|| Ok(())),
        }
    }

    /// Serve `served` and run the install against `claimed`, the asset the
    /// manifest describes. Returns the result and the progress reports.
    fn run(
        bundle: &Path,
        served: &[u8],
        claimed: &DesktopAsset,
        steps: Steps,
    ) -> (Result<(), InstallError>, Vec<Progress>) {
        let stub = Stub::start();
        stub.set(PATH, served.to_vec());
        let (tx, rx) = async_channel::unbounded();
        let url = format!("{}{PATH}", stub.base());
        let result = install(
            bundle,
            "0.6.1-nightly.1",
            &url,
            claimed,
            Duration::from_secs(30),
            tx,
            steps,
        );
        let mut reports = Vec::new();
        while let Ok(report) = rx.try_recv() {
            reports.push(report);
        }
        (result, reports)
    }

    #[test]
    fn macos_bundle_is_replaced_in_place() {
        let (root, bundle) = installed();
        let apps = root.path().join("Applications");
        let archive = bundle_archive("new", &incompressible(300 * 1024));
        let events = Arc::new(Mutex::new(Vec::new()));
        let steps = {
            let (e1, b1) = (events.clone(), bundle.clone());
            let (e2, b2) = (events.clone(), bundle.clone());
            Steps {
                codesign: Arc::new(move |_| {
                    e1.lock()
                        .unwrap()
                        .push(format!("codesign (installed: {})", version(&b1)));
                    Ok(())
                }),
                stop_agents: Arc::new(move || {
                    e2.lock()
                        .unwrap()
                        .push(format!("stop agents (installed: {})", version(&b2)));
                    Ok(())
                }),
            }
        };

        let (result, reports) = run(&bundle, &archive, &asset_of(FILE, &archive), steps);

        result.unwrap();
        assert_eq!(version(&bundle), "new");
        // The new executable kept its mode.
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(bundle.join(EXECUTABLE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        // Checked, then agents stopped, both while the old version was still
        // installed.
        assert_eq!(
            *events.lock().unwrap(),
            ["codesign (installed: old)", "stop agents (installed: old)"]
        );
        // Nothing left next to the bundle.
        assert_eq!(names(&apps), [BUNDLE]);
        // Progress went from the start to the total without going back.
        let total = archive.len() as u64;
        assert!(reports.len() >= 3, "{reports:?}");
        assert_eq!(
            reports.last().map(|r| (r.received, r.total)),
            Some((total, total))
        );
        assert!(
            reports
                .windows(2)
                .all(|pair| pair[0].received <= pair[1].received)
        );
    }

    #[test]
    fn failed_install_keeps_the_old_version() {
        let archive = bundle_archive("new", &[]);
        let claimed = asset_of(FILE, &archive);

        // The agents did not stop.
        let (root, bundle) = installed();
        let steps = Steps {
            codesign: Arc::new(|_| Ok(())),
            stop_agents: Arc::new(|| Err("agent `local-qmtcode` is still running".into())),
        };
        let (result, _) = run(&bundle, &archive, &claimed, steps);
        let err = result.unwrap_err();
        assert!(err.0.contains("still running"), "{err}");
        assert_eq!(version(&bundle), "old");
        assert_eq!(names(&root.path().join("Applications")), [BUNDLE]);

        // The new bundle is not signed: nothing was stopped either.
        let (root, bundle) = installed();
        let stopped = Arc::new(Mutex::new(false));
        let flag = stopped.clone();
        let steps = Steps {
            codesign: Arc::new(|_| Err("a sealed resource is missing".into())),
            stop_agents: Arc::new(move || {
                *flag.lock().unwrap() = true;
                Ok(())
            }),
        };
        let (result, _) = run(&bundle, &archive, &claimed, steps);
        let err = result.unwrap_err();
        assert!(err.0.contains("signature"), "{err}");
        assert_eq!(version(&bundle), "old");
        assert_eq!(names(&root.path().join("Applications")), [BUNDLE]);
        assert!(
            !*stopped.lock().unwrap(),
            "agents were stopped for a bad bundle"
        );
    }

    #[test]
    fn corrupted_download_leaves_no_files() {
        let good = bundle_archive("new", &[]);
        let mut no_exe = builder();
        append(
            &mut no_exe,
            &format!("{BUNDLE}/Contents/Info.plist"),
            0o644,
            b"<plist/>",
        );
        let no_exe = finish(no_exe);
        let mut not_runnable = builder();
        append(
            &mut not_runnable,
            &format!("{BUNDLE}/Contents/Info.plist"),
            0o644,
            b"<plist/>",
        );
        append(
            &mut not_runnable,
            &format!("{BUNDLE}/{EXECUTABLE}"),
            0o644,
            b"data",
        );
        let not_runnable = finish(not_runnable);

        // (what is served, what the manifest says, expected words in the error)
        let wrong_hash = DesktopAsset {
            sha256: "0".repeat(64),
            ..asset_of(FILE, &good)
        };
        let wrong_size = DesktopAsset {
            size: good.len() as u64 + 1,
            ..asset_of(FILE, &good)
        };
        let truncated = good[..good.len() / 2].to_vec();
        let html = b"<html>Not found</html>".to_vec();
        let cases: Vec<(&str, Vec<u8>, DesktopAsset, &str)> = vec![
            ("wrong sha256", good.clone(), wrong_hash, "SHA-256"),
            ("wrong size", good, wrong_size, "bytes"),
            (
                "truncated gzip",
                truncated.clone(),
                asset_of(FILE, &truncated),
                "archive",
            ),
            ("html page", html.clone(), asset_of(FILE, &html), "archive"),
            (
                "no executable",
                no_exe.clone(),
                asset_of(FILE, &no_exe),
                "runnable",
            ),
            (
                "executable without the x bit",
                not_runnable.clone(),
                asset_of(FILE, &not_runnable),
                "runnable",
            ),
        ];
        for (what, served, claimed, words) in cases {
            let (root, bundle) = installed();
            let stopped = Arc::new(Mutex::new(false));
            let flag = stopped.clone();
            let steps = Steps {
                codesign: Arc::new(|_| Ok(())),
                stop_agents: Arc::new(move || {
                    *flag.lock().unwrap() = true;
                    Ok(())
                }),
            };
            let (result, _) = run(&bundle, &served, &claimed, steps);
            let err = result
                .err()
                .unwrap_or_else(|| panic!("{what} was accepted"));
            assert!(
                err.0.to_lowercase().contains(&words.to_lowercase()),
                "{what}: {err}"
            );
            assert_eq!(version(&bundle), "old", "{what}");
            assert_eq!(names(&root.path().join("Applications")), [BUNDLE], "{what}");
            assert!(
                !*stopped.lock().unwrap(),
                "{what}: agents stopped for a bad download"
            );
        }
    }

    /// Entries that could write outside the bundle are refused before
    /// anything is extracted.
    #[test]
    fn unsafe_archive_entries_are_rejected() {
        fn with_name_field(tar: &mut Tar, name: &str) {
            // `Builder` refuses such a path, so write the name field directly.
            let mut header = tar::Header::new_gnu();
            header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
            header.set_size(4);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, &b"evil"[..]).unwrap();
        }
        fn link(tar: &mut Tar, kind: tar::EntryType, target: &str) {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_size(0);
            header.set_mode(0o755);
            tar.append_link(&mut header, format!("{BUNDLE}/Contents/link"), target)
                .unwrap();
        }

        // (what it is, how to add it to the archive, words in the error)
        type AddEvil = Box<dyn Fn(&mut Tar)>;
        let cases: Vec<(&str, AddEvil, &str)> = vec![
            (
                "dotdot",
                Box::new(|t| with_name_field(t, "../evil.txt")),
                "not inside",
            ),
            (
                "absolute",
                Box::new(|t| with_name_field(t, "/tmp/qmt-evil.txt")),
                "not inside",
            ),
            (
                "dotdot inside the bundle path",
                Box::new(|t| with_name_field(t, &format!("{BUNDLE}/../../evil.txt"))),
                "not inside",
            ),
            (
                "another bundle",
                Box::new(|t| with_name_field(t, "Other.app/Contents/Info.plist")),
                "not inside",
            ),
            (
                "absolute symlink",
                Box::new(|t| link(t, tar::EntryType::Symlink, "/etc/passwd")),
                "outside the bundle",
            ),
            (
                "outward symlink",
                Box::new(|t| link(t, tar::EntryType::Symlink, "../../../outside")),
                "outside the bundle",
            ),
            (
                "hard link",
                Box::new(|t| link(t, tar::EntryType::Link, "Contents/Info.plist")),
                "hard link",
            ),
        ];
        for (what, add_evil, words) in cases {
            let mut tar = builder();
            append(
                &mut tar,
                &format!("{BUNDLE}/Contents/Info.plist"),
                0o644,
                b"<plist/>",
            );
            append(
                &mut tar,
                &format!("{BUNDLE}/{EXECUTABLE}"),
                0o755,
                b"#!/bin/sh\n",
            );
            add_evil(&mut tar);
            let bytes = finish(tar);

            let (root, bundle) = installed();
            let (result, _) = run(&bundle, &bytes, &asset_of(FILE, &bytes), ok_steps());
            let err = result
                .err()
                .unwrap_or_else(|| panic!("{what} was accepted"));
            assert!(err.0.contains(words), "{what}: {err}");
            assert_eq!(version(&bundle), "old", "{what}");
            assert_eq!(names(&root.path().join("Applications")), [BUNDLE], "{what}");
            assert!(!Path::new("/tmp/qmt-evil.txt").exists(), "{what}");
            assert!(!root.path().join("evil.txt").exists(), "{what}");
        }
    }

    /// The production signature check, on a real (ad-hoc signed) bundle: a
    /// signed one passes; an unsigned one and one changed after signing do not.
    #[test]
    fn the_real_signature_check_tells_signed_from_unsigned() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join(BUNDLE);
        fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        fs::write(
            bundle.join("Contents/Info.plist"),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <plist version=\"1.0\"><dict>\
             <key>CFBundleExecutable</key><string>querymt-gpui</string>\
             <key>CFBundleIdentifier</key><string>mt.query.desktop.test</string>\
             <key>CFBundlePackageType</key><string>APPL</string>\
             </dict></plist>",
        )
        .unwrap();
        fs::copy("/usr/bin/true", bundle.join(EXECUTABLE)).unwrap();

        assert!(codesign(&bundle).is_err(), "unsigned bundle accepted");
        let signed = std::process::Command::new("codesign")
            .args(["--force", "--sign", "-", "--timestamp=none"])
            .arg(&bundle)
            .output()
            .unwrap();
        assert!(signed.status.success(), "{signed:?}");
        codesign(&bundle).unwrap();

        fs::write(bundle.join("Contents/Info.plist"), "<plist/>").unwrap();
        assert!(codesign(&bundle).is_err(), "tampered bundle accepted");
    }
}

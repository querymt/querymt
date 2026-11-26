//! Self-update state of the app: what the last check found, the schedule that
//! runs checks, and the preferences. The policy is in `crate::update` (pure
//! functions); this wires it to the network and the UI.
//!
//! Development builds (no release version) start no scheduler and make no
//! update request. The web build has no updater at all.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use async_channel::Sender;
use gpui_kit::*;

use super::{AgentsStore, ConnState};
use crate::update::install::{self, restart};
use crate::update::{
    BuildChannel, Channel, build_channel, check,
    fetch::{self, Feed, Progress},
    install_kind::InstallKind,
    install_kind::InstallProbe,
    install_kind::NotifyReason,
    platform_key,
    schedule::Scheduler,
    schedule::Trigger,
    selected_channel, verify,
};

/// How long the local agents get to stop before an install gives up (and the
/// installed version stays as it is).
const STOP_TIMEOUT: Duration = Duration::from_secs(20);

/// The updater's state, kept by the store.
pub struct Updates {
    /// What Settings and the sidebar show.
    pub state: check::UpdateState,
    scheduler: Scheduler,
    started: Instant,
    /// Added to the time since start; tests move the clock with it.
    skew: Duration,
    feed: Feed,
    public_key: String,
    build: BuildChannel,
    running: String,
    install_kind: Option<InstallKind>,
    /// Agents that were connected when an install stopped them; they are
    /// connected again when the install fails or the user keeps working.
    stopped: Vec<String>,
    /// The scheduler loop; dropping it stops the schedule.
    schedule: Option<Task<()>>,
    /// Counts the changes that make what a running check will find moot (a
    /// channel switch, a reset). A check that ends under another number is
    /// dropped.
    generation: u64,
    /// A channel switch asked for a check while another one was running: the
    /// next end of a check starts it.
    recheck: bool,
}

impl Default for Updates {
    fn default() -> Self {
        Self {
            state: check::UpdateState::Idle,
            scheduler: Scheduler::default(),
            started: Instant::now(),
            skew: Duration::ZERO,
            feed: Feed::default(),
            public_key: verify::EMBEDDED_PUBLIC_KEY.to_owned(),
            build: build_channel(),
            running: crate::util::APP_VERSION.to_owned(),
            install_kind: None,
            stopped: Vec::new(),
            schedule: None,
            generation: 0,
            recheck: false,
        }
    }
}

impl Updates {
    /// What was found, and what a running check will find, no longer applies
    /// (the channel changed, or the preferences were reset). A download or
    /// install that is already under way is left to finish.
    pub(super) fn invalidate(&mut self) {
        self.generation += 1;
        let busy = matches!(
            self.state,
            check::UpdateState::Downloading(_)
                | check::UpdateState::Installing
                | check::UpdateState::ReadyToRestart { .. }
        );
        if !busy {
            self.state = check::UpdateState::Idle;
        }
    }

    fn since_start(&self) -> Duration {
        self.started.elapsed() + self.skew
    }

    pub fn checking(&self) -> bool {
        self.scheduler.in_flight()
    }
}

/// What the user can do with the offered version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OfferAction {
    /// The app downloads and installs it. `closes_app`: the install ends with
    /// the app quitting (the Windows setup), so the button says so.
    Install { closes_app: bool },
    /// The app shows the version and a link only.
    NotifyOnly(NotifyReason),
}

impl AgentsStore {
    /// The channel to check, or `None` for a build that cannot update itself.
    pub fn update_channel(&self) -> Option<Channel> {
        selected_channel(self.prefs.updates.channel, self.updates.build)
    }

    /// The channel Settings shows as selected: the user's choice, else the
    /// build's.
    pub fn shown_update_channel(&self) -> Channel {
        self.prefs
            .updates
            .channel
            .unwrap_or(match self.updates.build {
                BuildChannel::Nightly => Channel::Nightly,
                _ => Channel::Stable,
            })
    }

    /// Whether this build checks for updates at all (Settings says "not
    /// available for this build" otherwise).
    pub fn updates_available(&self) -> bool {
        self.update_channel().is_some()
    }

    pub fn running_version(&self) -> &str {
        &self.updates.running
    }

    /// How this install replaces itself, read once from the system.
    pub fn update_install_kind(&mut self) -> InstallKind {
        if self.updates.install_kind.is_none() {
            let kind = InstallProbe::detect_current()
                .map(|probe| crate::update::install_kind::detect_install_kind(&probe))
                .unwrap_or(InstallKind::NotifyOnly(NotifyReason::Unmanaged));
            self.updates.install_kind = Some(kind);
        }
        self.updates.install_kind.clone().expect("set above")
    }

    /// What to offer for the current state's version.
    pub fn offer_action(&mut self) -> OfferAction {
        match self.update_install_kind() {
            InstallKind::NotifyOnly(reason) => OfferAction::NotifyOnly(reason),
            // Only the Windows setup needs the app out of the way.
            InstallKind::Nsis { .. } => OfferAction::Install { closes_app: true },
            _ => OfferAction::Install { closes_app: false },
        }
    }

    /// A release build installed as `kind`, for UI tests.
    #[cfg(test)]
    pub(crate) fn configure_updates_for_test(&mut self, build: BuildChannel, kind: InstallKind) {
        self.updates.build = build;
        self.updates.running = "0.6.0".to_owned();
        self.updates.install_kind = Some(kind);
    }

    /// Start the schedule: the first check 30 seconds after start, then every
    /// 6 hours. Development builds start none.
    pub(super) fn start_update_scheduler(&mut self, cx: &mut Context<Self>) {
        if !self.updates_available() {
            return;
        }
        self.updates.schedule = Some(cx.spawn(async move |this, cx| {
            loop {
                let wait = this.update(cx, |s, _| {
                    let since = s.updates.since_start();
                    s.updates
                        .scheduler
                        .wait_until_due(s.prefs.updates.auto_check, since)
                });
                let Ok(wait) = wait else { break };
                if wait.is_zero() {
                    let started = this.update(cx, |s, cx| s.check_updates(false, cx));
                    match started {
                        Err(_) => break,
                        // Not started (a check is running): look again later.
                        Ok(false) => {
                            cx.background_executor()
                                .timer(crate::update::schedule::MAX_TICK)
                                .await
                        }
                        Ok(true) => {}
                    }
                } else {
                    cx.background_executor().timer(wait).await;
                }
            }
        }));
    }

    /// Check for a newer release. `manual`: the user asked, so a skipped
    /// version is shown and a failure is reported. Returns whether a check
    /// started: not when this build cannot update, a check is running, or an
    /// automatic check is not due.
    pub fn check_updates(&mut self, manual: bool, cx: &mut Context<Self>) -> bool {
        let Some(channel) = self.update_channel() else {
            return false;
        };
        let trigger = if manual {
            Trigger::Manual
        } else {
            Trigger::Auto
        };
        let since = self.updates.since_start();
        if !self
            .updates
            .scheduler
            .start(trigger, self.prefs.updates.auto_check, since)
        {
            return false;
        }
        cx.notify();
        let feed = self.updates.feed.clone();
        let key = self.updates.public_key.clone();
        let running = self.updates.running.clone();
        let skipped = self.prefs.updates.skipped_version.clone();
        let generation = self.updates.generation;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    check::run_check(&feed, &key, channel, &running, skipped.as_deref(), manual)
                })
                .await;
            let _ = this.update(cx, |s, cx| s.finish_check(&result, manual, generation, cx));
        })
        .detach();
        true
    }

    fn finish_check(
        &mut self,
        result: &check::CheckResult,
        manual: bool,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        self.updates.scheduler.finish();
        if generation != self.updates.generation {
            // Asked for another channel, or before a reset: this answer is
            // about something else. Look again if a switch wanted a check.
            if std::mem::take(&mut self.updates.recheck) {
                self.check_updates(true, cx);
            }
            cx.notify();
            return;
        }
        let transition = check::after_check(result, manual, platform_key(), &self.updates.feed);
        if let Some(line) = &transition.log {
            tracing::warn!(target: "update", "{line}");
        }
        // A download or install in progress is not replaced by a check.
        let busy = matches!(
            self.updates.state,
            check::UpdateState::Downloading(_)
                | check::UpdateState::Installing
                | check::UpdateState::ReadyToRestart { .. }
        );
        if let (Some(state), false) = (transition.state, busy) {
            self.updates.state = state;
        }
        self.prefs.updates.last_check = Some(unix_seconds());
        self.save();
        cx.notify();
    }

    /// Do not announce the offered version again on automatic checks.
    pub fn skip_version(&mut self, cx: &mut Context<Self>) {
        if let check::UpdateState::Available(offer) = &self.updates.state {
            self.prefs.updates.skipped_version = Some(offer.version.clone());
            self.updates.state = check::UpdateState::UpToDate;
            self.save();
            cx.notify();
        }
    }

    /// Switch channel. The next check is the user's own, so it reports.
    pub fn set_update_channel(&mut self, channel: Channel, cx: &mut Context<Self>) {
        if self.prefs.updates.channel == Some(channel) {
            return;
        }
        self.prefs.updates.channel = Some(channel);
        // What was offered, and what a running check will find, belongs to
        // the other channel.
        self.updates.invalidate();
        self.save();
        cx.notify();
        if !self.check_updates(true, cx) {
            // A check is running (for the old channel): look again when it ends.
            self.updates.recheck = true;
        }
    }

    pub fn set_auto_check(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_prefs(|p| p.updates.auto_check = on, cx);
    }

    /// Whether a download or an install is running or waiting for a restart:
    /// no new check or install should start meanwhile.
    pub fn update_busy(&self) -> bool {
        matches!(
            self.updates.state,
            check::UpdateState::Downloading(_)
                | check::UpdateState::Installing
                | check::UpdateState::ReadyToRestart { .. }
        )
    }

    /// Download the offered version and install it, once the user asked for
    /// it. Nothing is downloaded before this (spec: no download without
    /// consent). Any failure ends in `Failed` with the installed version
    /// untouched.
    ///
    /// macOS and AppImage end in `ReadyToRestart`; the new version starts on
    /// [`restart_to_update`](Self::restart_to_update). On Windows the setup
    /// runs after the app quits, so a success quits the app right here.
    pub fn install_update(&mut self, cx: &mut Context<Self>) {
        let check::UpdateState::Available(offer) = self.updates.state.clone() else {
            return;
        };
        // An offer of a channel that is no longer the selected one is moot.
        if Some(offer.channel) != self.update_channel() {
            self.updates.state = check::UpdateState::Idle;
            cx.notify();
            return;
        }
        let kind = self.update_install_kind();
        if matches!(kind, InstallKind::NotifyOnly(_)) {
            return;
        }
        let Some(asset) = offer.asset.clone() else {
            self.updates.state = check::UpdateState::Failed(
                "This release has no installer for this platform.".into(),
            );
            cx.notify();
            return;
        };
        self.updates.stopped.clear();
        let feed = self.updates.feed.clone();
        let url = feed.asset_url(offer.channel, &offer.version, &asset.file);
        self.updates.state = check::UpdateState::Downloading(Progress {
            received: 0,
            total: asset.size,
        });
        cx.notify();

        // Progress reaches the card as it arrives; the loop ends when the
        // installer drops its sender.
        let (progress_tx, progress_rx) = async_channel::unbounded::<Progress>();
        cx.spawn(async move |this, cx| {
            while let Ok(report) = progress_rx.recv().await {
                let shown = this.update(cx, |s, cx| {
                    if matches!(s.updates.state, check::UpdateState::Downloading(_)) {
                        s.updates.state = check::UpdateState::Downloading(report);
                        cx.notify();
                    }
                });
                if shown.is_err() {
                    break;
                }
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let outcome = run_install(&this, cx, kind, &offer, asset, feed, url, progress_tx).await;
            let _ = this.update(cx, |s, cx| s.finish_install(&offer, outcome, cx));
        })
        .detach();
    }

    fn finish_install(
        &mut self,
        offer: &check::Offer,
        outcome: Result<Installed, String>,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            Ok(Installed::Relaunching) => {}
            Ok(Installed::ReadyToRestart) => {
                self.updates.state = check::UpdateState::ReadyToRestart {
                    version: offer.version.clone(),
                };
                // The user may keep working until they restart.
                self.reconnect_stopped(cx);
            }
            Err(why) => {
                tracing::warn!(target: "update", "install failed: {why}");
                self.updates.state = check::UpdateState::Failed(why);
                self.reconnect_stopped(cx);
            }
        }
        cx.notify();
    }

    /// Connect the agents an install stopped.
    fn reconnect_stopped(&mut self, cx: &mut Context<Self>) {
        for id in std::mem::take(&mut self.updates.stopped) {
            self.connect(&id, cx);
        }
    }

    /// Start the installed version and quit (macOS and AppImage), after the
    /// user chose "Restart now". The local agents are stopped first. If the
    /// new version cannot be started the app stays and says so.
    pub fn restart_to_update(&mut self, cx: &mut Context<Self>) {
        let check::UpdateState::ReadyToRestart { .. } = self.updates.state else {
            return;
        };
        let Some(launch) = install::launch_for(&self.update_install_kind()) else {
            return;
        };
        let stop = self.stop_local_agents(STOP_TIMEOUT, cx);
        cx.spawn(async move |this, cx| {
            // Quitting ends any agent that did not stop in time.
            let _ = stop.await;
            let _ = this.update(cx, |s, cx| {
                let result = restart::relaunch(&mut restart::AppEffects { cx }, &launch);
                if let Err(err) = result {
                    s.updates.state = check::UpdateState::Failed(format!(
                        "The update is installed, but the new version could not be started: {err} \
                         Open the app again to use it."
                    ));
                    s.reconnect_stopped(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Disconnect every local process agent and wait for each child to exit,
    /// so an install can replace files (a running `qmtcode` holds its own
    /// executable open; on Windows that blocks the replacement).
    ///
    /// The returned task resolves to the ids of agents still running when
    /// `timeout` passed; an install must not go ahead while that is not
    /// empty. The wait is on a background thread, not the UI thread.
    ///
    /// An agent that is still starting (spawned, not yet recorded) is
    /// disconnected like the others, but there is nothing to wait on; its
    /// child is killed by the connection's own teardown.
    pub fn stop_local_agents(
        &mut self,
        timeout: Duration,
        cx: &mut Context<Self>,
    ) -> Task<Vec<String>> {
        let ids: Vec<String> = self
            .configs
            .iter()
            .filter(|config| matches!(config.endpoint, super::Endpoint::Process { .. }))
            .map(|config| config.id.clone())
            .collect();
        let mut waits = Vec::new();
        for id in ids {
            // Take the signal first: `disconnect` forgets it.
            let exit = self.runtime.get_mut(&id).and_then(|rt| rt.exit.take());
            let was_up = self
                .runtime
                .get(&id)
                .is_some_and(|rt| matches!(rt.state, ConnState::Connected | ConnState::Connecting));
            if was_up && !self.updates.stopped.contains(&id) {
                self.updates.stopped.push(id.clone());
            }
            self.disconnect(&id, cx);
            if let Some(exit) = exit {
                waits.push((id, exit));
            }
        }
        cx.background_executor().spawn(async move {
            let deadline = Instant::now() + timeout;
            waits
                .into_iter()
                .filter(|(_, exit)| !exit.wait(deadline.saturating_duration_since(Instant::now())))
                .map(|(id, _)| id)
                .collect()
        })
    }
}

/// How an install ended when it did not fail.
enum Installed {
    /// Installed; the new version starts on restart.
    ReadyToRestart,
    /// The Windows setup was started and the app is quitting.
    Relaunching,
}

/// Download `url` to `dest` on its own thread, checking size and SHA-256.
#[cfg_attr(target_os = "macos", allow(dead_code))]
async fn download_to(
    cx: &AsyncApp,
    feed: Feed,
    url: String,
    asset: verify::DesktopAsset,
    dest: PathBuf,
    progress: Sender<Progress>,
) -> Result<(), String> {
    let handle = fetch::spawn_download(feed, url, asset, dest, progress);
    match cx
        .background_executor()
        .spawn(async move { handle.join() })
        .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(err.to_string()),
        Err(_) => Err("The download stopped unexpectedly.".into()),
    }
}

/// Stop the local agents and wait for them; an error names the ones that did
/// not stop, and the caller must not change any installed file.
#[cfg_attr(target_os = "macos", allow(dead_code))]
async fn stop_agents(this: &WeakEntity<AgentsStore>, cx: &mut AsyncApp) -> Result<(), String> {
    let stop = this
        .update(cx, |s, cx| {
            s.updates.state = check::UpdateState::Installing;
            cx.notify();
            s.stop_local_agents(STOP_TIMEOUT, cx)
        })
        .map_err(|_| "The app is closing.".to_string())?;
    let still_running = stop.await;
    if still_running.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "These agents did not stop: {}.",
            still_running.join(", ")
        ))
    }
}

/// The install of `kind`, from the download to the point where the new
/// version is in place (or, on Windows, the setup has been started).
#[allow(clippy::too_many_arguments)]
async fn run_install(
    this: &WeakEntity<AgentsStore>,
    cx: &mut AsyncApp,
    kind: InstallKind,
    offer: &check::Offer,
    asset: verify::DesktopAsset,
    feed: Feed,
    url: String,
    progress: Sender<Progress>,
) -> Result<Installed, String> {
    match kind {
        InstallKind::NotifyOnly(_) => Err("This install is not updated by the app.".into()),
        #[cfg(unix)]
        InstallKind::AppImage { path } => {
            use install::appimage;
            let target = PathBuf::from(path);
            let staged = appimage::staging_path(&target, std::process::id());
            download_to(cx, feed, url, asset, staged.clone(), progress).await?;
            // Checked and executable; deleted again if it is not installed.
            let prepared = appimage::prepare(&target, &staged).map_err(|e| e.0)?;
            stop_agents(this, cx).await?;
            prepared.commit(&appimage::Hooks::real()).map_err(|e| e.0)?;
            Ok(Installed::ReadyToRestart)
        }
        #[cfg(not(unix))]
        InstallKind::AppImage { .. } => Err("AppImage updates are not available here.".into()),
        #[cfg(target_os = "macos")]
        InstallKind::MacApp { bundle } => {
            use install::macos;
            // The crate calls this from its install thread, between checking
            // the new bundle and replacing the old one: ask the UI thread to
            // stop the agents and wait for the answer.
            let (ask, asked) = async_channel::bounded::<Sender<Result<(), String>>>(1);
            let stop: macos::Step = std::sync::Arc::new(move || {
                let closing = || "The app is closing.".to_string();
                let (reply, answer) = async_channel::bounded(1);
                ask.send_blocking(reply).map_err(|_| closing())?;
                answer.recv_blocking().map_err(|_| closing())?
            });
            let steps = macos::Steps::real(stop);
            let bundle = PathBuf::from(bundle);
            let version = offer.version.clone();
            let timeout = feed.download_timeout;
            // Its own thread: it blocks for the whole download, and it waits
            // for the UI thread's answer to the stop request below.
            let (done, finished) = async_channel::bounded(1);
            std::thread::Builder::new()
                .name("update-install".into())
                .spawn(move || {
                    let result =
                        macos::install(&bundle, &version, &url, &asset, timeout, progress, steps);
                    let _ = done.send_blocking(result);
                })
                .map_err(|err| format!("Could not start the install: {err}"))?;
            // Ends when the install thread no longer holds the sender.
            while let Ok(reply) = asked.recv().await {
                let answer = stop_agents(this, cx).await;
                let _ = reply.send(answer).await;
            }
            finished
                .recv()
                .await
                .map_err(|_| "The install stopped unexpectedly.".to_string())?
                .map_err(|e| e.0)?;
            Ok(Installed::ReadyToRestart)
        }
        #[cfg(not(target_os = "macos"))]
        InstallKind::MacApp { .. } => Err("macOS updates are not available here.".into()),
        InstallKind::Nsis { dir } => {
            use install::windows;
            let setup = windows::setup_path(&std::env::temp_dir(), &offer.version);
            download_to(cx, feed, url, asset, setup.clone(), progress).await?;
            windows::check_setup(&setup).map_err(|e| e.0)?;
            if let Err(why) = stop_agents(this, cx).await {
                let _ = std::fs::remove_file(&setup);
                return Err(why);
            }
            let launch = windows::launch(&setup, &dir);
            let started = this
                .update(cx, |_, cx| {
                    restart::relaunch(&mut restart::AppEffects { cx }, &launch)
                })
                .map_err(|_| "The app is closing.".to_string())?;
            if let Err(err) = started {
                let _ = std::fs::remove_file(&setup);
                return Err(err.0);
            }
            Ok(Installed::Relaunching)
        }
    }
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    // Not `use super::*`: it would glob-import gpui's `test` macro over the
    // built-in `#[test]` that `#[gpui_kit::test]` expands to.
    use gpui_kit::{AppContext as _, Entity};
    use sha2::{Digest, Sha256};

    use crate::store::{AgentsStore, Endpoint, Persisted, automations};
    use crate::transport::Exit;
    use crate::update::check::UpdateState;
    use crate::update::install_kind::InstallKind;
    use crate::update::test_stub::Stub;
    use crate::update::verify::test_signing::TestKey;
    use crate::update::{BuildChannel, Channel, fetch::Feed, selected_channel};

    const RUNNING: &str = "0.6.0";
    const NEWER: &str = "0.7.0";
    const ASSET_FILE: &str = "QueryMT-Desktop-v0.7.0-linux-x86_64.AppImage";

    fn publish_stable(stub: &Stub, key: &TestKey, version: &str) {
        publish_stable_with(stub, key, version, ASSET_FILE, b"hello");
    }

    /// A signed stable manifest whose every platform names `file` (`body`'s
    /// size and SHA-256), with `body` served as that file.
    fn publish_stable_with(stub: &Stub, key: &TestKey, version: &str, file: &str, body: &[u8]) {
        let asset = serde_json::json!({
            "file": file,
            "size": body.len(),
            "sha256": format!("{:x}", Sha256::digest(body)),
        });
        let manifest = serde_json::json!({
            "version": version,
            "channel": "stable",
            "commit": "0".repeat(40),
            "desktop_updates": {
                "linux-x86_64": asset,
                "darwin-aarch64": asset,
                "darwin-x86_64": asset,
                "windows-x86_64": asset,
                "linux-aarch64": asset,
            }
        })
        .to_string()
        .into_bytes();
        let signature = key.sign(&manifest, &format!("version:{version} channel:stable"));
        stub.set("/latest/download/release.json", manifest);
        stub.set("/latest/download/release.json.minisig", signature);
        stub.set(&format!("/download/v{version}/{file}"), body.to_vec());
    }

    /// A store for a build of `build`, with its feed and key replaced.
    fn store_for(
        cx: &mut gpui_kit::TestAppContext,
        build: BuildChannel,
        stub: &Stub,
        key: &TestKey,
    ) -> Entity<AgentsStore> {
        let base = stub.base();
        let public = key.public();
        cx.new(move |_| {
            let mut store = automations::schedule_ui_fixture();
            store.updates.build = build;
            store.updates.running = RUNNING.to_owned();
            store.updates.feed = Feed::new(&base);
            store.updates.public_key = public;
            store
        })
    }

    #[gpui_kit::test]
    fn development_build_makes_no_update_request(cx: &mut gpui_kit::TestAppContext) {
        let stub = Stub::start();
        let key = TestKey::new(7);
        publish_stable(&stub, &key, NEWER);
        let store = store_for(cx, BuildChannel::Dev, &stub, &key);
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                // The scheduler is not started, even with automatic checks on.
                assert!(s.prefs.updates.auto_check);
                s.start_update_scheduler(cx);
                assert!(s.updates.schedule.is_none());
                // Nor can a check be started by hand.
                assert!(!s.check_updates(false, cx));
                assert!(!s.check_updates(true, cx));
                assert!(!s.updates_available());
            });
        });
        // Well past the first-check delay.
        cx.executor().advance_clock(Duration::from_secs(120));
        cx.run_until_parked();
        assert_eq!(stub.hit_count(), 0, "{:?}", stub.hits());
        cx.update(|cx| assert_eq!(store.read(cx).updates.state, UpdateState::Idle));
    }

    #[gpui_kit::test]
    fn no_download_without_consent(cx: &mut gpui_kit::TestAppContext) {
        let stub = Stub::start();
        let key = TestKey::new(7);
        publish_stable(&stub, &key, NEWER);
        let store = store_for(cx, BuildChannel::Stable, &stub, &key);
        cx.update(|cx| assert!(store.update(cx, |s, cx| s.check_updates(true, cx))));
        cx.run_until_parked();
        cx.update(|cx| match &store.read(cx).updates.state {
            UpdateState::Available(offer) => assert_eq!(offer.version, NEWER),
            other => panic!("expected an offer, got {other:?}"),
        });
        // Only the manifest and its signature were requested.
        assert_eq!(
            stub.hits(),
            [
                "/latest/download/release.json",
                "/latest/download/release.json.minisig"
            ]
        );
        assert!(!stub.hits().iter().any(|p| p.contains(ASSET_FILE)));
    }

    #[gpui_kit::test]
    fn the_schedule_checks_after_the_first_delay(cx: &mut gpui_kit::TestAppContext) {
        let stub = Stub::start();
        let key = TestKey::new(7);
        publish_stable(&stub, &key, NEWER);
        let store = store_for(cx, BuildChannel::Stable, &stub, &key);
        cx.update(|cx| store.update(cx, |s, cx| s.start_update_scheduler(cx)));
        cx.run_until_parked();
        assert_eq!(stub.hit_count(), 0, "nothing before the first delay");
        // 30 seconds later the first check runs.
        cx.update(|cx| store.update(cx, |s, _| s.updates.skew = Duration::from_secs(31)));
        cx.executor().advance_clock(Duration::from_secs(61));
        cx.run_until_parked();
        assert_eq!(stub.hit_count(), 2, "{:?}", stub.hits());
        cx.update(|cx| {
            assert!(matches!(
                store.read(cx).updates.state,
                UpdateState::Available(_)
            ));
            assert!(store.read(cx).prefs.updates.last_check.is_some());
        });
    }

    #[gpui_kit::test]
    fn automatic_checks_off_make_no_request(cx: &mut gpui_kit::TestAppContext) {
        let stub = Stub::start();
        let key = TestKey::new(7);
        publish_stable(&stub, &key, NEWER);
        let store = store_for(cx, BuildChannel::Stable, &stub, &key);
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.set_auto_check(false, cx);
                s.updates.skew = Duration::from_secs(10_000);
                s.start_update_scheduler(cx);
            });
        });
        cx.executor().advance_clock(Duration::from_secs(600));
        cx.run_until_parked();
        assert_eq!(stub.hit_count(), 0);
        // "Check now" still works.
        cx.update(|cx| assert!(store.update(cx, |s, cx| s.check_updates(true, cx))));
        cx.run_until_parked();
        assert_eq!(stub.hit_count(), 2);
    }

    #[gpui_kit::test]
    fn concurrent_check_is_ignored_by_the_store(cx: &mut gpui_kit::TestAppContext) {
        let stub = Stub::start();
        let key = TestKey::new(7);
        publish_stable(&stub, &key, NEWER);
        let store = store_for(cx, BuildChannel::Stable, &stub, &key);
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                assert!(s.check_updates(true, cx));
                assert!(s.updates.checking());
                assert!(!s.check_updates(true, cx), "a second check does not start");
            });
        });
        cx.run_until_parked();
        assert_eq!(stub.hit_count(), 2, "one check, two requests");
        cx.update(|cx| assert!(!store.read(cx).updates.checking()));
    }

    #[gpui_kit::test]
    fn skipping_a_version_keeps_automatic_checks_quiet(cx: &mut gpui_kit::TestAppContext) {
        let stub = Stub::start();
        let key = TestKey::new(7);
        publish_stable(&stub, &key, NEWER);
        let store = store_for(cx, BuildChannel::Stable, &stub, &key);
        cx.update(|cx| store.update(cx, |s, cx| s.check_updates(true, cx)));
        cx.run_until_parked();
        cx.update(|cx| store.update(cx, |s, cx| s.skip_version(cx)));
        cx.update(|cx| {
            let s = store.read(cx);
            assert_eq!(s.prefs.updates.skipped_version.as_deref(), Some(NEWER));
            assert_eq!(s.updates.state, UpdateState::UpToDate);
        });
        // An automatic check finds it and announces nothing.
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.updates.skew = Duration::from_secs(30_000);
                assert!(s.check_updates(false, cx));
            })
        });
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(store.read(cx).updates.state, UpdateState::UpToDate));
        // A manual check still shows it.
        cx.update(|cx| store.update(cx, |s, cx| s.check_updates(true, cx)));
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(matches!(
                store.read(cx).updates.state,
                UpdateState::Available(_)
            ));
        });
    }

    #[gpui_kit::test]
    fn a_failed_automatic_check_is_quiet_and_a_manual_one_is_shown(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        let key = TestKey::new(7);
        let stub = Stub::start(); // serves nothing: every request is a 404
        let store = store_for(cx, BuildChannel::Stable, &stub, &key);
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.updates.skew = Duration::from_secs(10_000);
                assert!(s.check_updates(false, cx));
            })
        });
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(store.read(cx).updates.state, UpdateState::Idle));
        cx.update(|cx| store.update(cx, |s, cx| s.check_updates(true, cx)));
        cx.run_until_parked();
        cx.update(|cx| match &store.read(cx).updates.state {
            UpdateState::Failed(why) => assert!(why.contains("404"), "{why}"),
            other => panic!("{other:?}"),
        });
    }

    #[gpui_kit::test]
    fn preferences_survive_restart(cx: &mut gpui_kit::TestAppContext) {
        let stub = Stub::start();
        let key = TestKey::new(7);
        let store = store_for(cx, BuildChannel::Stable, &stub, &key);
        // Nightly, automatic checks off, and a skipped version.
        let saved = cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.set_update_channel(Channel::Nightly, cx);
                s.set_auto_check(false, cx);
                s.update_prefs(|p| p.updates.skipped_version = Some("0.7.0".into()), cx);
            });
            let s = store.read(cx);
            Persisted {
                configs: s.configs.clone(),
                prefs: s.prefs.clone(),
            }
        });
        // What the next start reads back.
        let text = serde_json::to_string(&saved).unwrap();
        let reloaded: Persisted = serde_json::from_str(&text).unwrap();
        assert_eq!(reloaded.prefs.updates.channel, Some(Channel::Nightly));
        assert!(!reloaded.prefs.updates.auto_check);
        assert_eq!(
            reloaded.prefs.updates.skipped_version.as_deref(),
            Some("0.7.0")
        );
        // And the new store follows the saved channel, whatever the build's.
        let (_, prefs, _) = crate::store::startup_state(Some(reloaded), Vec::new());
        assert_eq!(
            selected_channel(prefs.updates.channel, BuildChannel::Stable),
            Some(Channel::Nightly)
        );
        let _ = stub;
    }

    #[gpui_kit::test]
    fn switching_channel_clears_the_offer_and_checks(cx: &mut gpui_kit::TestAppContext) {
        let stub = Stub::start();
        let key = TestKey::new(7);
        publish_stable(&stub, &key, NEWER);
        let store = store_for(cx, BuildChannel::Stable, &stub, &key);
        cx.update(|cx| store.update(cx, |s, cx| s.check_updates(true, cx)));
        cx.run_until_parked();
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                assert!(matches!(s.updates.state, UpdateState::Available(_)));
                s.set_update_channel(Channel::Nightly, cx);
                assert_eq!(s.updates.state, UpdateState::Idle);
                assert_eq!(s.shown_update_channel(), Channel::Nightly);
            })
        });
        cx.run_until_parked();
        // The nightly feed was asked (it has nothing published: a 404).
        assert!(
            stub.hits()
                .iter()
                .any(|p| p == "/download/nightly/release.json")
        );
    }

    #[gpui_kit::test]
    fn a_check_that_ends_after_a_channel_switch_is_dropped(cx: &mut gpui_kit::TestAppContext) {
        let stub = Stub::start();
        let key = TestKey::new(7);
        publish_stable(&stub, &key, NEWER);
        let store = store_for(cx, BuildChannel::Stable, &stub, &key);
        // The stable check is running when the user switches to nightly.
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                assert!(s.check_updates(true, cx));
                s.set_update_channel(Channel::Nightly, cx);
                assert_eq!(s.updates.state, UpdateState::Idle);
            })
        });
        cx.run_until_parked();
        // The stable answer is not shown; nightly was asked once the stable
        // check ended (it has nothing published: a 404, shown as the user's).
        cx.update(|cx| match &store.read(cx).updates.state {
            UpdateState::Failed(why) => assert!(why.contains("404"), "{why}"),
            other => panic!("expected the nightly check's result, got {other:?}"),
        });
        assert!(
            stub.hits()
                .iter()
                .any(|p| p == "/download/nightly/release.json")
        );
        cx.update(|cx| assert!(!store.read(cx).updates.checking()));
    }

    #[gpui_kit::test]
    fn resetting_app_data_drops_the_offer_and_a_running_check(cx: &mut gpui_kit::TestAppContext) {
        let stub = Stub::start();
        let key = TestKey::new(7);
        publish_stable(&stub, &key, NEWER);
        let store = store_for(cx, BuildChannel::Stable, &stub, &key);
        cx.update(|cx| store.update(cx, |s, cx| s.check_updates(true, cx)));
        cx.run_until_parked();
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                assert!(matches!(s.updates.state, UpdateState::Available(_)));
                s.reset_with_seeds(Vec::new(), cx);
                assert_eq!(s.updates.state, UpdateState::Idle, "the offer is gone");
                assert!(s.prefs.updates.channel.is_none());
            })
        });

        // A check that was running when the data was reset does not bring the
        // old answer back.
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                assert!(s.check_updates(true, cx));
                s.reset_with_seeds(Vec::new(), cx);
            })
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let s = store.read(cx);
            assert_eq!(s.updates.state, UpdateState::Idle);
            assert!(!s.updates.checking());
        });
    }

    #[cfg(unix)]
    #[gpui_kit::test]
    fn an_offer_of_another_channel_is_not_installed(cx: &mut gpui_kit::TestAppContext) {
        let stub = Stub::start();
        let key = TestKey::new(7);
        publish_stable(&stub, &key, NEWER);
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("QueryMT.AppImage");
        std::fs::write(&target, b"\x7fELF the old appimage").unwrap();
        let kind = InstallKind::AppImage {
            path: target.to_str().unwrap().to_owned(),
        };
        let store = store_with_offer(cx, &stub, &key, kind);
        // The selected channel changed behind the offer's back.
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.prefs.updates.channel = Some(Channel::Nightly);
                s.install_update(cx);
            })
        });
        cx.run_until_parked();
        assert_eq!(state_of(cx, &store), UpdateState::Idle);
        let asset = format!("/download/v{NEWER}/{ASSET_FILE}");
        assert!(!stub.hits().contains(&asset), "nothing was downloaded");
        assert_eq!(names(dir.path()), ["QueryMT.AppImage"]);
    }

    /// A store whose only agent is a local process running `command`.
    fn store_running(cx: &mut gpui_kit::TestAppContext, command: String) -> Entity<AgentsStore> {
        cx.new(move |_| {
            let mut store = automations::schedule_ui_fixture();
            store.configs[0].endpoint = Endpoint::Process { command };
            store
        })
    }

    /// Start `stop_local_agents` and run it to the end.
    fn stop_and_collect(
        cx: &mut gpui_kit::TestAppContext,
        store: &Entity<AgentsStore>,
        timeout: Duration,
    ) -> Vec<String> {
        let slot = Rc::new(RefCell::new(None));
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                let stop = s.stop_local_agents(timeout, cx);
                let slot = slot.clone();
                cx.spawn(async move |_, _| {
                    *slot.borrow_mut() = Some(stop.await);
                })
                .detach();
            })
        });
        cx.run_until_parked();
        slot.borrow_mut().take().expect("the stop finished")
    }

    /// The fake agent is a real child process of the test. The install step
    /// (a rename, standing in for the first file change) runs only after the
    /// stop resolved, and by then the child must be gone.
    #[cfg(unix)]
    #[gpui_kit::test]
    fn local_agents_stop_before_files_change(cx: &mut gpui_kit::TestAppContext) {
        use std::os::unix::fs::PermissionsExt as _;

        // The real subprocess transport wakes GPUI tasks from its I/O threads.
        cx.executor().allow_parking();

        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent.sh");
        std::fs::write(&agent, "#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let store = store_running(cx, agent.to_str().unwrap().to_owned());

        cx.update(|cx| store.update(cx, |s, cx| s.connect("test-agent", cx)));
        cx.run_until_parked();
        let pid = cx.update(|cx| {
            let rt = &store.read(cx).runtime["test-agent"];
            assert!(rt.exit.is_some(), "the agent process was not started");
            rt.pid.expect("a pid")
        });
        let alive = |pid: u32| {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        assert!(alive(pid));

        let still_running = stop_and_collect(cx, &store, Duration::from_secs(20));
        assert!(still_running.is_empty(), "{still_running:?}");

        // The install's first rename.
        let agent_alive_at_rename = alive(pid);
        std::fs::rename(&agent, dir.path().join("agent.sh.old")).unwrap();
        assert!(!agent_alive_at_rename, "the agent outlived the stop");
    }

    #[gpui_kit::test]
    fn an_agent_that_does_not_stop_is_reported(cx: &mut gpui_kit::TestAppContext) {
        let store = store_running(cx, String::new());
        // A signal nothing will ever set: an agent that ignores its teardown.
        cx.update(|cx| {
            store.update(cx, |s, _| {
                s.runtime.get_mut("test-agent").unwrap().exit = Some(Exit::default());
            })
        });
        let still_running = stop_and_collect(cx, &store, Duration::from_millis(50));
        assert_eq!(still_running, ["test-agent"]);
    }

    // ---- install_update ----

    /// Run until the download and install are over. They use real threads, so
    /// the scheduler alone does not see them finish.
    #[cfg(unix)]
    fn settle(cx: &mut gpui_kit::TestAppContext, store: &Entity<AgentsStore>) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            cx.run_until_parked();
            let running = cx.update(|cx| {
                matches!(
                    store.read(cx).updates.state,
                    UpdateState::Downloading(_) | UpdateState::Installing
                )
            });
            if !running || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    fn state_of(cx: &mut gpui_kit::TestAppContext, store: &Entity<AgentsStore>) -> UpdateState {
        cx.update(|cx| store.read(cx).updates.state.clone())
    }

    /// A stable build that found `NEWER`, installed as `kind`.
    #[cfg(unix)]
    fn store_with_offer(
        cx: &mut gpui_kit::TestAppContext,
        stub: &Stub,
        key: &TestKey,
        kind: InstallKind,
    ) -> Entity<AgentsStore> {
        let store = store_for(cx, BuildChannel::Stable, stub, key);
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.updates.install_kind = Some(kind);
                assert!(s.check_updates(true, cx));
            })
        });
        cx.run_until_parked();
        assert!(matches!(state_of(cx, &store), UpdateState::Available(_)));
        store
    }

    #[cfg(unix)]
    fn names(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[cfg(unix)]
    fn alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    /// An agent script that runs until it is stopped.
    #[cfg(unix)]
    fn sleeping_agent(dir: &std::path::Path) -> String {
        use std::os::unix::fs::PermissionsExt as _;
        let agent = dir.join("agent.sh");
        std::fs::write(&agent, "#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        agent.to_str().unwrap().to_owned()
    }

    #[cfg(unix)]
    #[gpui_kit::test]
    fn install_update_downloads_and_installs_on_request(cx: &mut gpui_kit::TestAppContext) {
        // The download and install run on real threads that wake tasks.
        cx.executor().allow_parking();
        use std::os::unix::fs::PermissionsExt as _;

        let body = b"\x7fELF the new appimage".to_vec();
        let stub = Stub::start();
        let key = TestKey::new(7);
        publish_stable_with(&stub, &key, NEWER, ASSET_FILE, &body);
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("QueryMT.AppImage");
        std::fs::write(&target, b"\x7fELF the old appimage").unwrap();
        let kind = InstallKind::AppImage {
            path: target.to_str().unwrap().to_owned(),
        };
        let store = store_with_offer(cx, &stub, &key, kind);
        // Offered, not downloaded.
        assert!(!stub.hits().iter().any(|p| p.contains(ASSET_FILE)));

        cx.update(|cx| store.update(cx, |s, cx| s.install_update(cx)));
        assert!(matches!(state_of(cx, &store), UpdateState::Downloading(_)));
        settle(cx, &store);

        assert_eq!(
            state_of(cx, &store),
            UpdateState::ReadyToRestart {
                version: NEWER.into()
            }
        );
        assert_eq!(std::fs::read(&target).unwrap(), body);
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(names(dir.path()), ["QueryMT.AppImage"]);
        // One download, and only after the user asked.
        let downloads = stub
            .hits()
            .iter()
            .filter(|p| p.contains(ASSET_FILE))
            .count();
        assert_eq!(downloads, 1, "{:?}", stub.hits());
    }

    #[cfg(unix)]
    #[gpui_kit::test]
    fn a_failed_download_changes_nothing(cx: &mut gpui_kit::TestAppContext) {
        // The download and install run on real threads that wake tasks.
        cx.executor().allow_parking();
        let body = b"\x7fELF the new appimage".to_vec();
        let path = format!("/download/v{NEWER}/{ASSET_FILE}");
        // What the server does instead of serving the asset the manifest
        // describes: serve another file, or none.
        type Serve = Box<dyn Fn(&Stub, &str)>;
        let cases: Vec<(&str, Serve, &str)> = vec![
            (
                "replaced asset",
                Box::new(|stub, path| stub.set(path, b"\x7fELF the new appimagf".to_vec())),
                "SHA-256",
            ),
            (
                "missing asset",
                Box::new(|stub, path| stub.set_status(path, 404, b"gone".to_vec())),
                "404",
            ),
        ];
        for (what, break_it, words) in cases {
            let stub = Stub::start();
            let key = TestKey::new(7);
            publish_stable_with(&stub, &key, NEWER, ASSET_FILE, &body);
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("QueryMT.AppImage");
            std::fs::write(&target, b"\x7fELF the old appimage").unwrap();
            let kind = InstallKind::AppImage {
                path: target.to_str().unwrap().to_owned(),
            };
            let store = store_with_offer(cx, &stub, &key, kind);
            break_it(&stub, &path);

            cx.update(|cx| store.update(cx, |s, cx| s.install_update(cx)));
            settle(cx, &store);

            match state_of(cx, &store) {
                UpdateState::Failed(why) => assert!(why.contains(words), "{what}: {why}"),
                other => panic!("{what}: expected a failure, got {other:?}"),
            }
            assert_eq!(
                std::fs::read(&target).unwrap(),
                b"\x7fELF the old appimage",
                "{what}"
            );
            assert_eq!(names(dir.path()), ["QueryMT.AppImage"], "{what}");
        }
    }

    /// The stop and the reconnect around a failing install: the agent is
    /// stopped before the files are touched, and is back when the install
    /// failed.
    #[cfg(unix)]
    #[gpui_kit::test]
    fn a_failed_install_reconnects_the_agents_it_stopped(cx: &mut gpui_kit::TestAppContext) {
        // The download and install run on real threads that wake tasks.
        cx.executor().allow_parking();
        let body = b"\x7fELF the new appimage".to_vec();
        let stub = Stub::start();
        let key = TestKey::new(7);
        publish_stable_with(&stub, &key, NEWER, ASSET_FILE, &body);
        let dir = tempfile::tempdir().unwrap();
        let command = sleeping_agent(dir.path());
        // A non-empty directory where the file should be: the last step, the
        // rename over it, fails after the agents were stopped.
        let target = dir.path().join("app.AppImage");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keep"), b"x").unwrap();
        let kind = InstallKind::AppImage {
            path: target.to_str().unwrap().to_owned(),
        };
        let store = store_with_offer(cx, &stub, &key, kind);
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.configs[0].endpoint = Endpoint::Process { command };
                s.connect("test-agent", cx);
            })
        });
        cx.run_until_parked();
        let first = cx.update(|cx| store.read(cx).runtime["test-agent"].pid.expect("a pid"));
        assert!(alive(first));

        cx.update(|cx| store.update(cx, |s, cx| s.install_update(cx)));
        settle(cx, &store);

        match state_of(cx, &store) {
            UpdateState::Failed(why) => assert!(why.contains("Could not replace"), "{why}"),
            other => panic!("expected a failure, got {other:?}"),
        }
        assert!(!alive(first), "the first agent process was left running");
        let (second, has_exit) = cx.update(|cx| {
            let rt = &store.read(cx).runtime["test-agent"];
            (rt.pid, rt.exit.is_some())
        });
        assert!(has_exit, "the agent was not reconnected");
        assert!(second.is_some() && second != Some(first), "{second:?}");
        assert_eq!(names(dir.path()), ["agent.sh", "app.AppImage"]);
        assert!(target.join("keep").exists());
        // Do not leave the reconnected agent running.
        cx.update(|cx| store.update(cx, |s, cx| s.disconnect("test-agent", cx)));
    }

    /// The Windows arm, as far as a Unix host can run it: the setup is
    /// downloaded and checked, the agents are stopped, and starting it fails
    /// (it is not an executable file here). The setup is deleted, the app
    /// stays, says why, and has its agents back.
    #[cfg(unix)]
    #[gpui_kit::test]
    fn a_setup_that_cannot_start_is_removed_and_reported(cx: &mut gpui_kit::TestAppContext) {
        cx.executor().allow_parking();
        let setup_body = b"MZ\x90\x00 a windows setup".to_vec();
        let stub = Stub::start();
        let key = TestKey::new(7);
        publish_stable_with(
            &stub,
            &key,
            NEWER,
            "QueryMT-Desktop-v0.7.0-windows-x86_64-setup.exe",
            &setup_body,
        );
        let dir = tempfile::tempdir().unwrap();
        let command = sleeping_agent(dir.path());
        let kind = InstallKind::Nsis {
            dir: r"C:\Users\me\AppData\Local\QueryMT Desktop".into(),
        };
        let store = store_with_offer(cx, &stub, &key, kind);
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.configs[0].endpoint = Endpoint::Process { command };
                s.connect("test-agent", cx);
            })
        });
        cx.run_until_parked();
        let first = cx.update(|cx| store.read(cx).runtime["test-agent"].pid.expect("a pid"));

        cx.update(|cx| store.update(cx, |s, cx| s.install_update(cx)));
        settle(cx, &store);

        match state_of(cx, &store) {
            UpdateState::Failed(why) => assert!(why.contains("Could not start"), "{why}"),
            other => panic!("expected a failure, got {other:?}"),
        }
        let setup = crate::update::install::windows::setup_path(&std::env::temp_dir(), NEWER);
        assert!(!setup.exists(), "the setup was left in the temp directory");
        // Stopped for the install, connected again after it failed.
        assert!(!alive(first));
        let second = cx.update(|cx| store.read(cx).runtime["test-agent"].pid);
        assert!(second.is_some() && second != Some(first), "{second:?}");
        cx.update(|cx| store.update(cx, |s, cx| s.disconnect("test-agent", cx)));
    }

    /// The whole macOS path through the store: a real download, the real
    /// crate, a real ad-hoc signed bundle checked with the real `codesign`,
    /// and a running agent that is stopped first and connected again after.
    #[cfg(target_os = "macos")]
    #[gpui_kit::test]
    fn install_update_replaces_a_signed_mac_bundle(cx: &mut gpui_kit::TestAppContext) {
        // The download and install run on real threads that wake tasks.
        cx.executor().allow_parking();
        const BUNDLE: &str = "QueryMT Desktop.app";
        const FILE: &str = "QueryMT-Desktop-v0.7.0-macos-aarch64.app.tar.gz";

        // The new bundle, signed the way the release script signs it.
        let archive = {
            let root = tempfile::tempdir().unwrap();
            let bundle = root.path().join(BUNDLE);
            std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
            std::fs::create_dir_all(bundle.join("Contents/Resources")).unwrap();
            std::fs::write(
                bundle.join("Contents/Info.plist"),
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <plist version=\"1.0\"><dict>\
                 <key>CFBundleExecutable</key><string>querymt-gpui</string>\
                 <key>CFBundleIdentifier</key><string>mt.query.desktop.test</string>\
                 <key>CFBundlePackageType</key><string>APPL</string>\
                 </dict></plist>",
            )
            .unwrap();
            std::fs::copy("/usr/bin/true", bundle.join("Contents/MacOS/querymt-gpui")).unwrap();
            std::fs::write(bundle.join("Contents/Resources/version.txt"), "new").unwrap();
            let signed = std::process::Command::new("codesign")
                .args(["--force", "--sign", "-", "--timestamp=none"])
                .arg(&bundle)
                .output()
                .unwrap();
            assert!(signed.status.success(), "{signed:?}");
            let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
                Vec::new(),
                flate2::Compression::fast(),
            ));
            tar.follow_symlinks(false);
            tar.append_dir_all(BUNDLE, &bundle).unwrap();
            tar.into_inner().unwrap().finish().unwrap()
        };

        let stub = Stub::start();
        let key = TestKey::new(7);
        publish_stable_with(&stub, &key, NEWER, FILE, &archive);
        let root = tempfile::tempdir().unwrap();
        let apps = root.path().join("Applications");
        let installed = apps.join(BUNDLE);
        std::fs::create_dir_all(installed.join("Contents/Resources")).unwrap();
        std::fs::write(installed.join("Contents/Resources/version.txt"), "old").unwrap();
        let command = sleeping_agent(root.path());
        let kind = InstallKind::MacApp {
            bundle: installed.to_str().unwrap().to_owned(),
        };
        let store = store_with_offer(cx, &stub, &key, kind);
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.configs[0].endpoint = Endpoint::Process { command };
                s.connect("test-agent", cx);
            })
        });
        cx.run_until_parked();
        let first = cx.update(|cx| store.read(cx).runtime["test-agent"].pid.expect("a pid"));
        assert!(alive(first));

        cx.update(|cx| store.update(cx, |s, cx| s.install_update(cx)));
        settle(cx, &store);

        assert_eq!(
            state_of(cx, &store),
            UpdateState::ReadyToRestart {
                version: NEWER.into()
            }
        );
        let version = std::fs::read_to_string(installed.join("Contents/Resources/version.txt"));
        assert_eq!(version.unwrap(), "new");
        let verified = std::process::Command::new("codesign")
            .args(["--verify", "--strict"])
            .arg(&installed)
            .status()
            .unwrap();
        assert!(verified.success(), "the installed bundle's signature");
        assert_eq!(names(&apps), [BUNDLE]);
        // The agent was stopped for the swap and is connected again.
        assert!(!alive(first));
        let second = cx.update(|cx| store.read(cx).runtime["test-agent"].pid);
        assert!(second.is_some() && second != Some(first), "{second:?}");
        cx.update(|cx| store.update(cx, |s, cx| s.disconnect("test-agent", cx)));
    }
}

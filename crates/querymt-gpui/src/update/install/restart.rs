//! The last steps of an update: free the single-instance socket, start the
//! new version, quit (design D10, D11).
//!
//! The order matters. A process started while this one still listens on the
//! single-instance socket would hand its request to us and exit, and the app
//! would be gone with nothing running. So the socket is freed first.
//!
//! The local agents are stopped before this runs
//! (`AgentsStore::stop_local_agents` waits for them), because that wait is
//! asynchronous. The effects are injected so the order is tested without
//! starting anything.

use super::{InstallError, Launch};

/// What the restart does to the outside world.
pub trait Effects {
    /// Stop listening for later instances and free their socket. Returns once
    /// a later instance would find nobody.
    fn drop_listener(&mut self);
    /// Listen again, after a start that did not happen.
    fn restore_listener(&mut self);
    /// Start the new version (or, on Windows, the setup that installs it).
    fn spawn(&mut self, launch: &Launch) -> Result<(), InstallError>;
    /// End this process.
    fn quit(&mut self);
}

/// Start `launch` and quit. If it cannot be started the app stays, listening
/// again, and the error is returned for the Updates card to show.
pub fn relaunch(effects: &mut dyn Effects, launch: &Launch) -> Result<(), InstallError> {
    effects.drop_listener();
    if let Err(err) = effects.spawn(launch) {
        effects.restore_listener();
        return Err(err);
    }
    effects.quit();
    Ok(())
}

/// The effects of the running app: the quick panel's single-instance socket
/// (if the panel is enabled), a detached process, and the app's own quit.
pub struct AppEffects<'a> {
    pub cx: &'a mut gpui_kit::App,
}

impl Effects for AppEffects<'_> {
    fn drop_listener(&mut self) {
        if let Some(host) = crate::quick::QuickHost::global(self.cx) {
            host.update(self.cx, |host, _| host.release_instance_socket());
        }
    }

    fn restore_listener(&mut self) {
        if let Some(host) = crate::quick::QuickHost::global(self.cx) {
            host.update(self.cx, |host, _| host.restore_instance_socket());
        }
    }

    fn spawn(&mut self, launch: &Launch) -> Result<(), InstallError> {
        super::spawn_detached(launch).map_err(|err| {
            InstallError(format!(
                "Could not start `{}`: {err}",
                launch.program.display()
            ))
        })
    }

    fn quit(&mut self) {
        self.cx.quit();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::quick::instance::{Command, Endpoint, Listening, forward, listen};

    fn endpoint() -> Endpoint {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        Endpoint::named(
            format!("qmt-restart-test-{}-{n}", std::process::id()),
            std::env::temp_dir(),
        )
    }

    /// Records the order of the effects. Its `spawn` plays the new process: it
    /// starts as a second instance would, by forwarding to a running one.
    struct Recorder {
        events: Vec<&'static str>,
        endpoint: Endpoint,
        listening: Option<Listening>,
        signals: async_channel::Sender<crate::quick::Signal>,
        /// Whether the new process would have reached an old instance.
        reached_the_old_instance: Option<bool>,
        spawn_fails: bool,
    }

    impl Recorder {
        fn new() -> Self {
            let endpoint = endpoint();
            let (signals, _requests) = async_channel::bounded(8);
            let listening = listen(&endpoint, signals.clone()).expect("listen");
            Self {
                events: Vec::new(),
                endpoint,
                listening: Some(listening),
                signals,
                reached_the_old_instance: None,
                spawn_fails: false,
            }
        }
    }

    impl Effects for Recorder {
        fn drop_listener(&mut self) {
            self.events.push("listener dropped");
            if let Some(listening) = self.listening.take() {
                listening.release();
            }
        }

        fn restore_listener(&mut self) {
            self.events.push("listener restored");
            self.listening = listen(&self.endpoint, self.signals.clone()).ok();
        }

        fn spawn(&mut self, _launch: &Launch) -> Result<(), InstallError> {
            self.events.push("child spawned");
            self.reached_the_old_instance = Some(forward(&self.endpoint, Command::Show));
            if self.spawn_fails {
                Err(InstallError("no such file".into()))
            } else {
                Ok(())
            }
        }

        fn quit(&mut self) {
            self.events.push("quit");
        }
    }

    fn launch() -> Launch {
        Launch {
            program: "new-app".into(),
            args: Vec::new(),
            raw_tail: None,
        }
    }

    #[test]
    fn restart_opens_the_new_version() {
        let mut effects = Recorder::new();
        // Before the restart a second start reaches this instance, and would
        // exit.
        assert!(forward(&effects.endpoint, Command::Show));

        relaunch(&mut effects, &launch()).unwrap();

        assert_eq!(
            effects.events,
            ["listener dropped", "child spawned", "quit"],
            "the socket is freed before the new version starts, and quit is last"
        );
        // A second `forward` after the drop reaches nobody: the new version
        // starts as the first instance instead of handing off to this one.
        assert_eq!(effects.reached_the_old_instance, Some(false));
        assert!(!forward(&effects.endpoint, Command::Show));
    }

    #[test]
    fn a_start_that_fails_keeps_the_app_running_and_listening() {
        let mut effects = Recorder::new();
        effects.spawn_fails = true;

        let err = relaunch(&mut effects, &launch()).unwrap_err();

        assert_eq!(err.0, "no such file");
        assert_eq!(
            effects.events,
            ["listener dropped", "child spawned", "listener restored"],
            "no quit"
        );
        assert!(forward(&effects.endpoint, Command::Show), "listening again");
    }
}

//! One running instance per user (design D9).
//!
//! The first instance listens on a local socket named for the user. A later
//! one connects, sends a [`Command`] (`--quick` sends [`Command::Quick`],
//! a plain start sends [`Command::Show`]) and exits. When the connection
//! fails, no instance is running and this one starts.

use std::io::{self, BufRead as _, BufReader, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_channel::Sender;
use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, ListenerOptions, Name, Stream,
};

use super::Signal;

/// What a second instance asks the running one for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    /// Toggle the quick panel (`--quick`).
    Quick,
    /// Bring the main window forward.
    Show,
}

impl Command {
    fn wire(self) -> &'static str {
        match self {
            Self::Quick => "quick\n",
            Self::Show => "show\n",
        }
    }

    fn parse(line: &str) -> Option<Self> {
        match line.trim() {
            "quick" => Some(Self::Quick),
            "show" => Some(Self::Show),
            _ => None,
        }
    }

    fn signal(self) -> Signal {
        match self {
            Self::Quick => Signal::TogglePanel,
            Self::Show => Signal::OpenMain,
        }
    }
}

/// Where the instance socket lives.
#[derive(Clone, Debug)]
pub(crate) struct Endpoint {
    name: String,
    /// Directory of the socket file, where sockets are files (macOS).
    dir: PathBuf,
}

impl Endpoint {
    /// The socket of the current user.
    pub(crate) fn for_user() -> Self {
        let user: String = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "user".into())
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .collect();
        let dir = dirs::runtime_dir().unwrap_or_else(std::env::temp_dir);
        Self::named(format!("querymt-gpui-{user}"), dir)
    }

    pub(crate) fn named(name: String, dir: PathBuf) -> Self {
        Self { name, dir }
    }

    fn socket_path(&self) -> PathBuf {
        self.dir.join(format!("{}.sock", self.name))
    }

    /// Namespaced sockets (Linux, Windows pipes) where the platform has
    /// them, a socket file elsewhere.
    fn socket_name(&self) -> io::Result<Name<'static>> {
        if GenericNamespaced::is_supported() {
            self.name.clone().to_ns_name::<GenericNamespaced>()
        } else {
            self.socket_path().to_fs_name::<GenericFilePath>()
        }
    }

    /// A socket file nobody listens on any more.
    fn remove_stale_file(&self) {
        if !GenericNamespaced::is_supported() {
            let _ = std::fs::remove_file(self.socket_path());
        }
    }
}

/// Ask the running instance, if there is one, to do `command`. True when it
/// took the request, so this process should exit; false when no instance
/// answers and this one should start.
pub(crate) fn forward(endpoint: &Endpoint, command: Command) -> bool {
    let Ok(name) = endpoint.socket_name() else {
        return false;
    };
    let Ok(mut stream) = Stream::connect(name) else {
        return false;
    };
    stream
        .write_all(command.wire().as_bytes())
        .and_then(|()| stream.flush())
        .is_ok()
}

/// The listening socket. Dropping it stops the thread and frees the name.
pub(crate) struct Listening {
    endpoint: Endpoint,
    stop: Arc<AtomicBool>,
}

/// Listen for commands of later instances and send them as signals.
pub(crate) fn listen(endpoint: &Endpoint, signals: Sender<Signal>) -> io::Result<Listening> {
    // A file left by a crashed instance would make the bind fail.
    if Stream::connect(endpoint.socket_name()?).is_err() {
        endpoint.remove_stale_file();
    }
    let listener = ListenerOptions::new()
        .name(endpoint.socket_name()?)
        .create_sync()?;
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    std::thread::Builder::new()
        .name("quick-instance".into())
        .spawn(move || {
            for connection in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(connection) = connection else { continue };
                let mut line = String::new();
                if BufReader::new(connection).read_line(&mut line).is_err() {
                    continue;
                }
                if let Some(command) = Command::parse(&line) {
                    // A full queue already holds requests for the app.
                    let _ = signals.try_send(command.signal());
                }
            }
        })?;
    Ok(Listening {
        endpoint: endpoint.clone(),
        stop,
    })
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the thread out of `accept`; it sees the flag and ends.
        if let Ok(name) = self.endpoint.socket_name() {
            let _ = Stream::connect(name);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    use async_channel::Receiver;

    use super::{Command, Endpoint, Signal, forward, listen};

    fn endpoint() -> Endpoint {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Endpoint::named(
            format!("qmt-test-{}-{n}", std::process::id()),
            std::env::temp_dir(),
        )
    }

    fn next(requests: &Receiver<Signal>) -> Option<Signal> {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if let Ok(signal) = requests.try_recv() {
                return Some(signal);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    #[test]
    fn second_instance_forwards_quick_and_exits() {
        let endpoint = endpoint();
        // Nobody listens: the second instance starts as the first.
        assert!(!forward(&endpoint, Command::Quick));

        let (signals, requests) = async_channel::bounded(8);
        let listening = listen(&endpoint, signals).expect("listen");

        // `--quick` reaches the running instance, which toggles its panel,
        // and the second process has nothing left to do.
        assert!(forward(&endpoint, Command::Quick));
        assert!(matches!(next(&requests), Some(Signal::TogglePanel)));

        // A plain start asks for the main window.
        assert!(forward(&endpoint, Command::Show));
        assert!(matches!(next(&requests), Some(Signal::OpenMain)));

        // Once the first instance is gone, a new one starts again.
        drop(listening);
        std::thread::sleep(Duration::from_millis(100));
        assert!(!forward(&endpoint, Command::Quick));
    }

    /// A crashed instance leaves its socket file behind.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_stale_socket_file_does_not_block_the_next_instance() {
        let endpoint = endpoint();
        drop(std::os::unix::net::UnixListener::bind(endpoint.socket_path()).unwrap());
        assert!(endpoint.socket_path().exists());
        let (signals, requests) = async_channel::bounded(8);
        let listening = listen(&endpoint, signals).expect("listen over a stale file");
        assert!(forward(&endpoint, Command::Quick));
        assert!(matches!(next(&requests), Some(Signal::TogglePanel)));
        drop(listening);
    }

    #[test]
    fn unknown_commands_are_ignored() {
        assert_eq!(Command::parse("quick\n"), Some(Command::Quick));
        assert_eq!(Command::parse(" show "), Some(Command::Show));
        assert_eq!(Command::parse("restart"), None);
    }
}

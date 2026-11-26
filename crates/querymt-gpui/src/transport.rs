//! Byte pipes to an ACP agent. Every transport yields the same shape: a sender
//! for outgoing JSON-RPC lines and a receiver of inbound events.
//!
//! - native: WebSocket (`ws://`, `wss://`) on a background thread, or a local
//!   process speaking ACP over stdio (`qmtcode --acp`).
//! - web: the browser's `WebSocket`.
//!
//! Closing the outgoing sender (`Sender::close`) tears the transport down.

use async_channel::{Receiver, Sender};

#[derive(Clone, Debug)]
#[cfg_attr(target_family = "wasm", allow(dead_code))]
pub enum Target {
    /// Command line of a local ACP process (native only).
    Command(String),
    /// Full WebSocket URL, e.g. `ws://127.0.0.1:42069/acp/ws`.
    WebSocket(String),
}

#[cfg_attr(target_family = "wasm", allow(dead_code))]
pub enum Inbound {
    Message(String),
    /// A line the agent process wrote to stderr.
    Log(String),
    /// Something the transport itself reports (the process it started).
    Note(String),
    Closed(String),
}

pub struct Connection {
    pub outgoing: Sender<String>,
    pub incoming: Receiver<Inbound>,
    pub pid: Option<u32>,
    /// Set once a local agent process has exited and been reaped. `None` for
    /// a WebSocket, which has no process of ours.
    pub exit: Option<Exit>,
}

/// Tells whether a local agent process has exited. The thread that owns the
/// child marks it after `wait`; anyone else can wait for it with a timeout.
#[derive(Clone, Debug, Default)]
#[cfg_attr(target_family = "wasm", allow(dead_code))]
pub struct Exit(std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>);

#[cfg_attr(target_family = "wasm", allow(dead_code))]
impl Exit {
    pub fn mark(&self) {
        let (flag, wake) = &*self.0;
        *flag
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        wake.notify_all();
    }

    #[cfg(test)]
    pub fn has_exited(&self) -> bool {
        *self
            .0
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Block until the process has exited or `timeout` passes. Returns whether
    /// it exited. Blocks the calling thread, so keep it off the UI thread.
    pub fn wait(&self, timeout: std::time::Duration) -> bool {
        let (flag, wake) = &*self.0;
        let guard = flag
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (guard, _) = wake
            .wait_timeout_while(guard, timeout, |exited| !*exited)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard
    }
}

pub async fn connect(target: Target) -> Result<Connection, String> {
    #[cfg(not(target_family = "wasm"))]
    {
        native::connect(target).await
    }
    #[cfg(target_family = "wasm")]
    {
        web::connect(target).await
    }
}

#[cfg(not(target_family = "wasm"))]
mod native {
    use super::*;
    use async_channel::TryRecvError;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    use std::time::Duration;
    use tungstenite::{Message, stream::MaybeTlsStream};

    pub async fn connect(target: Target) -> Result<Connection, String> {
        let (out_tx, out_rx) = async_channel::unbounded::<String>();
        let (in_tx, in_rx) = async_channel::unbounded::<Inbound>();
        match target {
            Target::WebSocket(url) => {
                let (ready_tx, ready_rx) = futures::channel::oneshot::channel();
                std::thread::Builder::new()
                    .name("qmt-ws".into())
                    .spawn(move || ws_thread(url, out_rx, in_tx, ready_tx))
                    .map_err(|e| e.to_string())?;
                ready_rx
                    .await
                    .map_err(|_| "Connection thread exited.".to_string())??;
                Ok(Connection {
                    outgoing: out_tx,
                    incoming: in_rx,
                    pid: None,
                    exit: None,
                })
            }
            Target::Command(command_line) => {
                let (pid, exit) = spawn_process(&command_line, out_rx, in_tx)?;
                Ok(Connection {
                    outgoing: out_tx,
                    incoming: in_rx,
                    pid: Some(pid),
                    exit: Some(exit),
                })
            }
        }
    }

    fn ws_thread(
        url: String,
        out_rx: Receiver<String>,
        in_tx: Sender<Inbound>,
        ready: futures::channel::oneshot::Sender<Result<(), String>>,
    ) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut socket = match tungstenite::connect(url.as_str()) {
            Ok((socket, _)) => socket,
            Err(err) => {
                let _ = ready.send(Err(format!("Unable to connect to {url}: {err}")));
                return;
            }
        };
        // Short read timeout so the same thread can also flush outgoing frames.
        let timeout = Some(Duration::from_millis(15));
        let _ = match socket.get_ref() {
            MaybeTlsStream::Plain(tcp) => tcp.set_read_timeout(timeout),
            MaybeTlsStream::Rustls(tls) => tls.get_ref().set_read_timeout(timeout),
            _ => Ok(()),
        };
        let _ = ready.send(Ok(()));

        loop {
            loop {
                match out_rx.try_recv() {
                    Ok(text) => {
                        if let Err(err) = socket.send(Message::Text(text.into())) {
                            let _ = in_tx.send_blocking(Inbound::Closed(format!(
                                "WebSocket send failed: {err}"
                            )));
                            return;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Closed) => {
                        let _ = socket.close(None);
                        let _ = socket.flush();
                        return;
                    }
                }
            }
            match socket.read() {
                Ok(Message::Text(text)) => {
                    if in_tx
                        .send_blocking(Inbound::Message(text.as_str().to_owned()))
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(Message::Binary(bytes)) => {
                    if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                        let _ = in_tx.send_blocking(Inbound::Message(text));
                    }
                }
                Ok(Message::Close(frame)) => {
                    let reason = frame
                        .map(|f| {
                            format!(
                                "WebSocket closed: {} ({})",
                                f.reason.as_str(),
                                u16::from(f.code)
                            )
                        })
                        .unwrap_or_else(|| "WebSocket closed.".into());
                    let _ = in_tx.send_blocking(Inbound::Closed(reason));
                    return;
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(err))
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(err) => {
                    let _ = in_tx.send_blocking(Inbound::Closed(format!("WebSocket error: {err}")));
                    return;
                }
            }
        }
    }

    fn spawn_process(
        command_line: &str,
        out_rx: Receiver<String>,
        in_tx: Sender<Inbound>,
    ) -> Result<(u32, Exit), String> {
        let exe = std::env::current_exe().ok();
        let path_var = std::env::var_os("PATH");
        spawn_process_with(
            command_line,
            exe.as_deref(),
            path_var.as_deref(),
            out_rx,
            in_tx,
        )
    }

    /// `CREATE_NO_WINDOW`: a GUI app's agent process gets no console window.
    #[cfg(windows)]
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// The command for an agent process: stdio piped to the app and, on
    /// Windows, no console window.
    fn agent_command(program: &std::path::Path, args: &[String]) -> Command {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        command
    }

    /// The log entry for a started agent process: which copy, which path.
    fn start_note(resolved: &crate::local_agent::Resolved, command_line: &str, pid: u32) -> String {
        use crate::local_agent::Source;
        let path = resolved.path.display();
        match resolved.source {
            Source::Bundled => format!("Started bundled qmtcode `{path}` (pid {pid})."),
            Source::Path => format!("Started PATH qmtcode `{path}` (pid {pid})."),
            Source::AsWritten => format!("Started `{command_line}` (pid {pid})."),
        }
    }

    /// [`spawn_process`] over an injected app executable and PATH value.
    fn spawn_process_with(
        command_line: &str,
        exe: Option<&std::path::Path>,
        path_var: Option<&std::ffi::OsStr>,
        out_rx: Receiver<String>,
        in_tx: Sender<Inbound>,
    ) -> Result<(u32, Exit), String> {
        let parts = split_command_line(command_line)?;
        let (program, args) = parts.split_first().ok_or("Command line is empty.")?;
        let resolved = crate::local_agent::resolve_program(program, exe, path_var)?;
        let mut child = agent_command(&resolved.path, args)
            .spawn()
            .map_err(|err| format!("Failed to start `{}`: {err}", resolved.path.display()))?;
        let pid = child.id();
        let mut stdin = child.stdin.take().ok_or("No stdin")?;
        let stdout = child.stdout.take().ok_or("No stdout")?;
        let stderr = child.stderr.take().ok_or("No stderr")?;
        let _ = in_tx.try_send(Inbound::Note(start_note(&resolved, command_line, pid)));

        let tx = in_tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) if line.trim().is_empty() => {}
                    Ok(line) => {
                        if tx.send_blocking(Inbound::Message(line)).is_err() {
                            return;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send_blocking(Inbound::Closed("Agent process exited.".into()));
        });
        let tx = in_tx;
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = tx.send_blocking(Inbound::Log(line));
            }
        });
        let exit = Exit::default();
        let exited = exit.clone();
        std::thread::spawn(move || {
            while let Ok(line) = out_rx.recv_blocking() {
                if writeln!(stdin, "{line}")
                    .and_then(|_| stdin.flush())
                    .is_err()
                {
                    break;
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            exited.mark();
        });
        Ok((pid, exit))
    }

    /// Minimal shell-style splitting: whitespace, single and double quotes, backslash escapes.
    fn split_command_line(input: &str) -> Result<Vec<String>, String> {
        let mut parts = Vec::new();
        let mut current = String::new();
        let mut in_word = false;
        let mut quote: Option<char> = None;
        let mut chars = input.chars();
        while let Some(c) = chars.next() {
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                (Some('"'), '\\') => current.extend(chars.next()),
                (Some(_), c) => current.push(c),
                (None, '"' | '\'') => {
                    quote = Some(c);
                    in_word = true;
                }
                (None, '\\') => {
                    current.extend(chars.next());
                    in_word = true;
                }
                (None, c) if c.is_whitespace() => {
                    if in_word {
                        parts.push(std::mem::take(&mut current));
                        in_word = false;
                    }
                }
                (None, c) => {
                    current.push(c);
                    in_word = true;
                }
            }
        }
        if quote.is_some() {
            return Err("Unterminated quote in command line.".into());
        }
        if in_word {
            parts.push(current);
        }
        Ok(parts)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[cfg(unix)]
        fn fake_qmtcode(path: &std::path::Path) {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "#!/bin/sh\necho started\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        /// The first note the production spawn path writes for `command_line`.
        #[cfg(unix)]
        fn first_note(
            command_line: &str,
            exe: &std::path::Path,
            path_var: &std::ffi::OsStr,
        ) -> (String, u32) {
            let (_out_tx, out_rx) = async_channel::unbounded::<String>();
            let (in_tx, in_rx) = async_channel::unbounded::<Inbound>();
            let (pid, _exit) =
                spawn_process_with(command_line, Some(exe), Some(path_var), out_rx, in_tx).unwrap();
            loop {
                match in_rx.recv_blocking().unwrap() {
                    Inbound::Note(note) => return (note, pid),
                    Inbound::Closed(why) => panic!("closed before a note: {why}"),
                    _ => {}
                }
            }
        }

        #[cfg(unix)]
        #[test]
        fn log_shows_bundled_copy() {
            let root = tempfile::tempdir().unwrap();
            let bundled = root.path().join("app").join("qmtcode");
            let on_path = root.path().join("bin").join("qmtcode");
            fake_qmtcode(&bundled);
            fake_qmtcode(&on_path);
            let path_var = std::env::join_paths([root.path().join("bin")]).unwrap();
            let exe = root.path().join("app").join("querymt-gpui");

            let (note, pid) = first_note("qmtcode --acp", &exe, &path_var);
            assert_eq!(
                note,
                format!(
                    "Started bundled qmtcode `{}` (pid {pid}).",
                    bundled.display()
                )
            );
        }

        #[cfg(unix)]
        #[test]
        fn log_shows_path_copy() {
            let root = tempfile::tempdir().unwrap();
            let on_path = root.path().join("bin").join("qmtcode");
            fake_qmtcode(&on_path);
            let path_var = std::env::join_paths([root.path().join("bin")]).unwrap();
            let exe = root.path().join("app").join("querymt-gpui");

            let (note, pid) = first_note("qmtcode --acp", &exe, &path_var);
            assert_eq!(
                note,
                format!("Started PATH qmtcode `{}` (pid {pid}).", on_path.display())
            );
        }

        #[cfg(unix)]
        #[test]
        fn explicit_path_is_logged_as_written() {
            let root = tempfile::tempdir().unwrap();
            let explicit = root.path().join("opt").join("qmtcode");
            fake_qmtcode(&explicit);
            let exe = root.path().join("app").join("querymt-gpui");
            let command_line = format!("{} --acp", explicit.display());
            let (note, pid) = first_note(&command_line, &exe, std::ffi::OsStr::new(""));
            assert_eq!(note, format!("Started `{command_line}` (pid {pid})."));
        }

        #[test]
        fn missing_qmtcode_fails_to_start() {
            let (_out_tx, out_rx) = async_channel::unbounded::<String>();
            let (in_tx, _in_rx) = async_channel::unbounded::<Inbound>();
            let root = tempfile::tempdir().unwrap();
            let exe = root.path().join("querymt-gpui");
            let err = spawn_process_with(
                "qmtcode --acp",
                Some(&exe),
                Some(std::ffi::OsStr::new("")),
                out_rx,
                in_tx,
            )
            .unwrap_err();
            assert!(
                err.contains("qmtcode") && err.contains("not found"),
                "{err}"
            );
        }

        #[test]
        fn exit_wait_times_out_until_marked() {
            let exit = Exit::default();
            assert!(!exit.has_exited());
            assert!(!exit.wait(Duration::from_millis(20)));
            let waiter = exit.clone();
            let handle = std::thread::spawn(move || waiter.wait(Duration::from_secs(10)));
            exit.mark();
            assert!(handle.join().unwrap());
            assert!(exit.has_exited());
        }

        /// Closing the outgoing channel is what stops the agent; the exit
        /// signal is set only after the child was killed and reaped.
        #[cfg(unix)]
        #[test]
        fn closing_the_channel_stops_the_process_and_marks_it_exited() {
            let root = tempfile::tempdir().unwrap();
            let agent = root.path().join("agent.sh");
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::write(&agent, "#!/bin/sh\nexec sleep 60\n").unwrap();
                std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let (out_tx, out_rx) = async_channel::unbounded::<String>();
            let (in_tx, _in_rx) = async_channel::unbounded::<Inbound>();
            let exe = root.path().join("querymt-gpui");
            let (_pid, exit) = spawn_process_with(
                agent.to_str().unwrap(),
                Some(&exe),
                Some(std::ffi::OsStr::new("")),
                out_rx,
                in_tx,
            )
            .unwrap();
            assert!(!exit.has_exited());
            out_tx.close();
            assert!(exit.wait(Duration::from_secs(10)));
        }

        /// The creation flags cannot be read back from a `Command`; this
        /// checks the flag value and that the production builder still
        /// pipes all three standard streams.
        #[cfg(windows)]
        #[test]
        fn no_console_window_for_a_local_agent() {
            assert_eq!(CREATE_NO_WINDOW, 0x0800_0000);
            let args = ["/C".to_string(), "echo hello".to_string()];
            let child = agent_command(std::path::Path::new("cmd"), &args)
                .spawn()
                .unwrap();
            assert!(child.stdin.is_some() && child.stderr.is_some());
            let out = child.wait_with_output().unwrap();
            assert!(String::from_utf8_lossy(&out.stdout).contains("hello"));
        }
    }
}

#[cfg(target_family = "wasm")]
mod web {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    use wasm_bindgen::{JsCast, prelude::*};
    use web_sys::{CloseEvent, Event, MessageEvent, WebSocket};

    pub async fn connect(target: Target) -> Result<Connection, String> {
        let url = match target {
            Target::WebSocket(url) => url,
            Target::Command(_) => {
                return Err("Local processes cannot be started from the browser.".into());
            }
        };
        let ws = WebSocket::new(&url).map_err(|e| format!("Invalid WebSocket URL {url}: {e:?}"))?;
        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);
        let (out_tx, out_rx) = async_channel::unbounded::<String>();
        let (in_tx, in_rx) = async_channel::unbounded::<Inbound>();
        let (open_tx, open_rx) = futures::channel::oneshot::channel::<Result<(), String>>();
        let open_tx = Rc::new(RefCell::new(Some(open_tx)));

        let on_open = {
            let open_tx = open_tx.clone();
            Closure::<dyn FnMut()>::new(move || {
                if let Some(tx) = open_tx.borrow_mut().take() {
                    let _ = tx.send(Ok(()));
                }
            })
        };
        let on_error = {
            let (open_tx, in_tx, url) = (open_tx.clone(), in_tx.clone(), url.clone());
            Closure::<dyn FnMut(Event)>::new(move |_| {
                if let Some(tx) = open_tx.borrow_mut().take() {
                    let _ = tx.send(Err(format!("Unable to connect to {url}.")));
                } else {
                    let _ = in_tx.try_send(Inbound::Closed("WebSocket error.".into()));
                }
            })
        };
        let on_message = {
            let in_tx = in_tx.clone();
            Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                let data = event.data();
                let text = data.as_string().or_else(|| {
                    // Proxies may forward JSON-RPC as binary frames.
                    let buffer = data.dyn_into::<js_sys::ArrayBuffer>().ok()?;
                    String::from_utf8(js_sys::Uint8Array::new(&buffer).to_vec()).ok()
                });
                if let Some(text) = text {
                    let _ = in_tx.try_send(Inbound::Message(text));
                }
            })
        };
        let on_close = {
            let (open_tx, in_tx) = (open_tx.clone(), in_tx.clone());
            Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
                let reason = if event.reason().is_empty() {
                    format!("WebSocket closed (code {}).", event.code())
                } else {
                    format!("WebSocket closed: {}", event.reason())
                };
                if let Some(tx) = open_tx.borrow_mut().take() {
                    let _ = tx.send(Err(reason.clone()));
                }
                let _ = in_tx.try_send(Inbound::Closed(reason));
            })
        };
        ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        ws.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));

        let detach = |ws: &WebSocket| {
            ws.set_onopen(None);
            ws.set_onerror(None);
            ws.set_onmessage(None);
            ws.set_onclose(None);
            let _ = ws.close();
        };

        match open_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                detach(&ws);
                return Err(err);
            }
            Err(_) => {
                detach(&ws);
                return Err("Connection cancelled.".into());
            }
        }

        let socket = ws.clone();
        wasm_bindgen_futures::spawn_local(async move {
            // The closures must outlive the socket's use of them.
            let _closures = (on_open, on_error, on_message, on_close);
            while let Ok(text) = out_rx.recv().await {
                if socket.send_with_str(&text).is_err() {
                    break;
                }
            }
            detach(&socket);
        });
        drop(in_tx);
        Ok(Connection {
            outgoing: out_tx,
            incoming: in_rx,
            pid: None,
            exit: None,
        })
    }
}

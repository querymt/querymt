//! A local HTTP server for update tests: it serves fixed responses, counts the
//! requests it saw, and lets a test change a response between requests (a
//! nightly replaced during a download).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

type Routes = Arc<Mutex<HashMap<String, (u16, Vec<u8>)>>>;

pub struct Stub {
    addr: SocketAddr,
    routes: Routes,
    hits: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
}

impl Stub {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stub");
        let addr = listener.local_addr().unwrap();
        let routes: Routes = Arc::default();
        let hits: Arc<Mutex<Vec<String>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (routes, hits, stop) = (routes.clone(), hits.clone(), stop.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let (routes, hits) = (routes.clone(), hits.clone());
                    std::thread::spawn(move || serve(stream, &routes, &hits));
                }
            });
        }
        Self {
            addr,
            routes,
            hits,
            stop,
        }
    }

    /// The feed base URL (`http://127.0.0.1:<port>`).
    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Serve `body` with status 200 at `path`, or replace what is there.
    pub fn set(&self, path: &str, body: impl Into<Vec<u8>>) {
        self.set_status(path, 200, body);
    }

    pub fn set_status(&self, path: &str, status: u16, body: impl Into<Vec<u8>>) {
        self.routes
            .lock()
            .unwrap()
            .insert(path.to_owned(), (status, body.into()));
    }

    /// Every request path seen so far, in order.
    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }

    pub fn hit_count(&self) -> usize {
        self.hits.lock().unwrap().len()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so its thread ends.
        let _ = TcpStream::connect(self.addr);
    }
}

fn serve(stream: TcpStream, routes: &Routes, hits: &Arc<Mutex<Vec<String>>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    // Skip the headers.
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) if line == "\r\n" || line == "\n" => break,
            Ok(_) => {}
        }
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_owned();
    hits.lock().unwrap().push(path.clone());
    let (status, body) = routes
        .lock()
        .unwrap()
        .get(&path)
        .cloned()
        .unwrap_or((404, b"not found".to_vec()));
    let mut out = stream;
    let head = format!(
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = out.write_all(head.as_bytes());
    // In chunks, so a large body arrives as a stream.
    for chunk in body.chunks(16 * 1024) {
        if out.write_all(chunk).is_err() {
            return;
        }
    }
    let _ = out.flush();
}

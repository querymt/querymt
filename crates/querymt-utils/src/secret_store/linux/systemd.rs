//! User-scoped systemd credentials (Varlink, systemd >= 256). QueryMT stores
//! opaque ciphertext only; systemd owns the host key, TPM and encryption policy.

use base64::{Engine, prelude::BASE64_STANDARD};
use keyring_core::{
    Entry, Error, Result,
    api::{Credential, CredentialApi, CredentialStoreApi},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    any::Any,
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

const SOCKET: &str = "/run/systemd/io.systemd.Credentials";
const MAX_SECRET: usize = 1024 * 1024;
const MAX_MESSAGE: usize = 2 * 1024 * 1024;

#[derive(Clone)]
struct Client {
    socket: PathBuf,
    peer_uid: u32,
    timeout: Duration,
}

impl Client {
    fn call(&self, method: &str, mut parameters: Value) -> io::Result<Value> {
        let deadline = Instant::now() + self.timeout;
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
        socket.connect_timeout(
            &socket2::SockAddr::unix(&self.socket)?,
            remaining(deadline)?,
        )?;
        let mut stream: UnixStream = socket.into();
        if rustix::net::sockopt::socket_peercred(&stream)?.uid.as_raw() != self.peer_uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "systemd Credentials peer is not trusted",
            ));
        }
        // Explicit scope is mandatory: omitting it defaults to system scope and
        // can require polkit. No null/auto-initrd mode or interactive escalation.
        parameters["scope"] = json!("user");
        parameters["uid"] = json!(rustix::process::getuid().as_raw());
        parameters["allowInteractiveAuthentication"] = json!(false);
        // Keep the v256 field set. In particular, allowNull/withKey only exist in
        // v259+: explicit user scope already refuses unscoped/null envelopes.
        let mut request = serde_json::to_vec(
            &json!({"method": format!("io.systemd.Credentials.{method}"), "parameters": parameters}),
        )?;
        if request.len() >= MAX_MESSAGE {
            return Err(invalid("credential request is too large"));
        }
        request.push(0);
        let mut pending = request.as_slice();
        while !pending.is_empty() {
            stream.set_write_timeout(Some(remaining(deadline)?))?;
            let count = stream.write(pending)?;
            if count == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            pending = &pending[count..];
        }
        let mut response = Vec::new();
        loop {
            stream.set_read_timeout(Some(remaining(deadline)?))?;
            let mut bytes = [0; 8192];
            let count = stream.read(&mut bytes)?;
            if count == 0 {
                return Err(invalid("systemd Credentials closed an incomplete reply"));
            }
            let end = bytes[..count].iter().position(|byte| *byte == 0);
            response.extend_from_slice(&bytes[..end.unwrap_or(count)]);
            if response.len() > MAX_MESSAGE {
                return Err(invalid("credential reply is too large"));
            }
            if end.is_some() {
                break;
            }
        }
        let reply: Value = serde_json::from_slice(&response)
            .map_err(|_| invalid("invalid systemd Credentials reply"))?;
        if let Some(error) = reply.get("error").and_then(Value::as_str) {
            let kind =
                match error {
                    "io.systemd.Credentials.BadFormat"
                    | "io.systemd.Credentials.NameMismatch"
                    | "io.systemd.Credentials.TimeMismatch"
                    | "io.systemd.Credentials.BadScope"
                    | "io.systemd.Credentials.NullKeyNotAllowed"
                    | "io.systemd.Credentials.KeyBelongsToOtherTPM" => io::ErrorKind::InvalidData,
                    "io.systemd.InteractiveAuthenticationRequired"
                    | "io.systemd.PermissionDenied" => io::ErrorKind::PermissionDenied,
                    "org.varlink.service.InterfaceNotFound"
                    | "org.varlink.service.MethodNotFound" => io::ErrorKind::Unsupported,
                    _ => io::ErrorKind::Other,
                };
            // Never include reply parameters: they could contain secret data.
            return Err(io::Error::new(
                kind,
                format!("systemd Credentials: {error}"),
            ));
        }
        if reply.get("continues").and_then(Value::as_bool) == Some(true) {
            return Err(invalid("unexpected streaming credential reply"));
        }
        reply
            .get("parameters")
            .cloned()
            .ok_or_else(|| invalid("missing credential reply parameters"))
    }

    fn encrypt(&self, name: &str, data: &[u8]) -> io::Result<String> {
        if data.len() > MAX_SECRET {
            return Err(invalid("credential exceeds systemd's 1 MiB limit"));
        }
        let reply = self.call(
            "Encrypt",
            json!({"name": name, "data": BASE64_STANDARD.encode(data)}),
        )?;
        let blob = reply
            .get("blob")
            .and_then(Value::as_str)
            .filter(|blob| !blob.is_empty())
            .ok_or_else(|| invalid("missing encrypted credential blob"))?;
        // Validate the returned representation, but retain it verbatim on disk.
        BASE64_STANDARD
            .decode(blob)
            .map_err(|_| invalid("invalid encrypted credential encoding"))?;
        Ok(blob.to_owned())
    }

    fn decrypt(&self, name: &str, blob: &str) -> io::Result<Vec<u8>> {
        let reply = self.call("Decrypt", json!({"name": name, "blob": blob}))?;
        let encoded = reply
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("missing decrypted credential data"))?;
        let data = BASE64_STANDARD
            .decode(encoded)
            .map_err(|_| invalid("invalid decrypted credential encoding"))?;
        if data.len() > MAX_SECRET {
            return Err(invalid("decrypted credential exceeds systemd's size limit"));
        }
        Ok(data)
    }
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "systemd Credentials operation timed out",
            )
        })
}

#[derive(Clone)]
pub(super) struct Store {
    root: PathBuf,
    client: Client,
}

impl Store {
    pub(super) fn new(root: PathBuf) -> io::Result<Arc<Self>> {
        Self::open(
            root,
            Client {
                socket: SOCKET.into(),
                peer_uid: 0,
                timeout: Duration::from_secs(10),
            },
        )
    }

    fn open(root: PathBuf, client: Client) -> io::Result<Arc<Self>> {
        // Socket presence is not enough: check the caller's actual ability to
        // encrypt and decrypt, without writing probe data to the credential dir.
        let blob = client.encrypt("querymt-probe", b"querymt-probe")?;
        if client.decrypt("querymt-probe", &blob)? != b"querymt-probe" {
            return Err(invalid("systemd Credentials probe readback failed"));
        }
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)?;
        let meta = fs::symlink_metadata(&root)?;
        if !meta.is_dir()
            || meta.mode() & 0o077 != 0
            || meta.uid() != rustix::process::geteuid().as_raw()
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "credential directory must be owned by this user, private (0700), and not a symlink",
            ));
        }
        Ok(Arc::new(Self { root, client }))
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn platform(error: io::Error) -> Error {
    if error.kind() == io::ErrorKind::PermissionDenied {
        Error::NoStorageAccess(Box::new(error))
    } else {
        Error::PlatformFailure(Box::new(error))
    }
}
fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    options
}
fn lock(root: &Path) -> io::Result<File> {
    let file = private_options()
        .read(true)
        .write(true)
        .create(true)
        .open(root.join("lock"))?;
    file.lock()?;
    Ok(file)
}
fn read_private(path: &Path) -> io::Result<String> {
    let file = private_options().read(true).open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.mode() & 0o077 != 0
        || meta.uid() != rustix::process::geteuid().as_raw()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "credential must be a private regular file owned by this user",
        ));
    }
    if meta.len() > MAX_MESSAGE as u64 {
        return Err(invalid("credential file is too large"));
    }
    let mut blob = String::new();
    file.take(MAX_MESSAGE as u64 + 1)
        .read_to_string(&mut blob)?;
    if blob.len() > MAX_MESSAGE {
        return Err(invalid("credential file is too large"));
    }
    Ok(blob)
}
fn atomic_write(root: &Path, name: &str, blob: &str) -> io::Result<()> {
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let temporary = root.join(format!(".tmp-{:x}", u128::from_le_bytes(random)));
    let mut file = private_options()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(blob.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, root.join(name))?;
        File::open(root)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

impl CredentialStoreApi for Store {
    fn vendor(&self) -> String {
        "systemd Credentials".into()
    }
    fn id(&self) -> String {
        self.root.display().to_string()
    }
    fn build(
        &self,
        service: &str,
        user: &str,
        modifiers: Option<&HashMap<&str, &str>>,
    ) -> Result<Entry> {
        if modifiers.is_some_and(|mods| !mods.is_empty()) {
            return Err(Error::NotSupportedByStore(
                "systemd credential modifiers are not supported".into(),
            ));
        }
        let identity = serde_json::to_vec(&(service, user))
            .map_err(|error| platform(io::Error::other(error)))?;
        let hash: String = Sha256::digest(identity)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(Entry::new_with_credential(Arc::new(SystemdCredential {
            store: self.clone(),
            name: format!("querymt-{hash}.cred"),
            service: service.into(),
            user: user.into(),
        })))
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct SystemdCredential {
    store: Store,
    name: String,
    service: String,
    user: String,
}
impl CredentialApi for SystemdCredential {
    fn set_secret(&self, secret: &[u8]) -> Result<()> {
        let blob = self
            .store
            .client
            .encrypt(&self.name, secret)
            .map_err(platform)?;
        // No network calls while holding the cross-process filesystem lock.
        let _lock = lock(&self.store.root).map_err(platform)?;
        atomic_write(&self.store.root, &self.name, &blob).map_err(platform)
    }
    fn get_secret(&self) -> Result<Vec<u8>> {
        let blob = {
            let _lock = lock(&self.store.root).map_err(platform)?;
            read_private(&self.store.root.join(&self.name)).map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    Error::NoEntry
                } else {
                    platform(error)
                }
            })?
        };
        self.store
            .client
            .decrypt(&self.name, &blob)
            .map_err(platform)
    }
    fn delete_credential(&self) -> Result<()> {
        let _lock = lock(&self.store.root).map_err(platform)?;
        fs::remove_file(self.store.root.join(&self.name)).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                Error::NoEntry
            } else {
                platform(error)
            }
        })?;
        File::open(&self.store.root)
            .and_then(|dir| dir.sync_all())
            .map_err(platform)
    }
    fn get_credential(&self) -> Result<Option<Arc<Credential>>> {
        self.get_secret()?;
        Ok(None)
    }
    fn get_specifiers(&self) -> Option<(String, String)> {
        Some((self.service.clone(), self.user.clone()))
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn fragmented_reply_user_scope_and_name_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            for expected_method in ["Encrypt", "Decrypt"] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut frame = Vec::new();
                while !frame.ends_with(&[0]) {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    frame.push(byte[0]);
                }
                let request: Value = serde_json::from_slice(&frame[..frame.len() - 1]).unwrap();
                assert_eq!(
                    request["method"],
                    format!("io.systemd.Credentials.{expected_method}")
                );
                assert_eq!(request["parameters"]["scope"], "user");
                assert_eq!(
                    request["parameters"]["uid"],
                    rustix::process::getuid().as_raw()
                );
                assert_eq!(
                    request["parameters"]["allowInteractiveAuthentication"],
                    false
                );
                let reply = if expected_method == "Encrypt" {
                    assert_eq!(
                        BASE64_STANDARD
                            .decode(request["parameters"]["data"].as_str().unwrap())
                            .unwrap(),
                        b"binary\0test"
                    );
                    json!({"parameters":{"blob":BASE64_STANDARD.encode(b"opaque test fixture")}})
                } else {
                    json!({"error":"io.systemd.Credentials.NameMismatch"})
                };
                for byte in serde_json::to_vec(&reply).unwrap().into_iter().chain([0]) {
                    stream.write_all(&[byte]).unwrap();
                }
            }
        });
        let client = Client {
            socket: path,
            peer_uid: rustix::process::getuid().as_raw(),
            timeout: Duration::from_secs(2),
        };
        let blob = client.encrypt("fixture", b"binary\0test").unwrap();
        assert_eq!(
            client.decrypt("wrong", &blob).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        server.join().unwrap();
    }

    #[test]
    fn stalled_server_times_out_and_untrusted_peer_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = [0; 4096];
            while stream.read(&mut bytes).unwrap() != 0 {}
            let (mut stream, _) = listener.accept().unwrap();
            assert_eq!(stream.read(&mut bytes).unwrap(), 0); // no plaintext sent to untrusted peer
        });
        let mut client = Client {
            socket: path,
            peer_uid: rustix::process::getuid().as_raw(),
            timeout: Duration::from_millis(100),
        };
        let error = client.encrypt("fixture", b"test").unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
        client.peer_uid = u32::MAX;
        assert_eq!(
            client.encrypt("fixture", b"test").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        server.join().unwrap();
    }

    #[test]
    fn ciphertext_files_are_private_and_symlinks_are_not_followed() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        atomic_write(dir.path(), "fixture.cred", "opaque fixture").unwrap();
        let path = dir.path().join("fixture.cred");
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(read_private(&path).unwrap(), "opaque fixture");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            read_private(&path).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        symlink(&path, dir.path().join("symlink.cred")).unwrap();
        assert!(read_private(&dir.path().join("symlink.cred")).is_err());
    }

    #[test]
    #[ignore = "requires systemd Credentials; only touches QMT_TEST_SYSTEMD_ROOT or a temporary directory"]
    fn live_persistent_roundtrip_and_tamper_rejection() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::env::var_os("QMT_TEST_SYSTEMD_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| temp.path().join("credentials"));
        let store = Store::new(root.clone()).unwrap();
        let entry = store.build("querymt-test", "roundtrip", None).unwrap();
        match entry.get_secret() {
            Ok(value) => assert_eq!(value, b"persistent\0test secret"),
            Err(Error::NoEntry) => entry.set_secret(b"persistent\0test secret").unwrap(),
            Err(error) => panic!("could not read test credential: {error}"),
        }
        drop(entry);
        drop(store);
        let reopened = Store::new(root.clone()).unwrap();
        let entry = reopened.build("querymt-test", "roundtrip", None).unwrap();
        assert_eq!(entry.get_secret().unwrap(), b"persistent\0test secret");
        if std::env::var_os("QMT_TEST_SYSTEMD_ROOT").is_none() {
            let file = fs::read_dir(&root)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path.extension().is_some_and(|ext| ext == "cred"))
                .unwrap();
            let blob = fs::read(&file).unwrap();
            assert!(
                !blob
                    .windows(b"test secret".len())
                    .any(|bytes| bytes == b"test secret")
            );
            let mut ciphertext = BASE64_STANDARD.decode(&blob).unwrap();
            *ciphertext.last_mut().unwrap() ^= 1;
            fs::write(&file, BASE64_STANDARD.encode(ciphertext)).unwrap();
            assert!(entry.get_secret().is_err());
            entry.delete_credential().unwrap();
            assert!(matches!(entry.get_secret(), Err(Error::NoEntry)));
        }
    }
}

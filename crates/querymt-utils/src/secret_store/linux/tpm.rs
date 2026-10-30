//! TPM-bound encryption at rest. No PCR policy: normal boot updates do not
//! invalidate the store. Clearing/replacing the TPM does; never replace its DEK
//! automatically. purecrypto-tpm has no salted/encrypted transport sessions yet,
//! so this does not protect against physical TPM-bus interception or processes
//! already authorized to read these files and use this TPM.

use std::{
    any::Any,
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use keyring_core::{
    Entry, Error, Result,
    api::{Credential, CredentialApi, CredentialStoreApi},
};
use purecrypto_tpm::{
    Auth, Tpm,
    marshal::{Marshal, Reader, Unmarshal},
    transport::{DeviceTransport, Transport},
    types::{
        constants::{Alg, rh},
        handles::Handle,
        structures::{Buffer, Public, SensitiveCreate},
    },
};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

const KEY_FILE: &str = "sealed-key";
const KEY_HEADER: &[u8] = b"QMTTPM01";
const SECRET_HEADER: &[u8] = b"QMTSEC01";
const MAX_FILE_SIZE: u64 = 16 * 1024 * 1024;

struct Inner {
    root: PathBuf,
    key: Zeroizing<Vec<u8>>,
}

pub(super) struct Store(Arc<Inner>);

impl Store {
    pub(super) fn new(root: PathBuf) -> io::Result<Arc<Self>> {
        let transport = DeviceTransport::open_default().map_err(io::Error::other)?;
        Self::open(root, transport)
    }

    fn open(root: PathBuf, transport: impl Transport) -> io::Result<Arc<Self>> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)?;
        let meta = fs::symlink_metadata(&root)?;
        if !meta.is_dir() || meta.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "TPM store must be a private directory (0700), not a symlink",
            ));
        }
        let _lock = lock(&root)?;
        let mut tpm = Tpm::new(transport);
        // Keep this template stable: it deterministically recreates the parent
        // under the unchanged owner hierarchy seed, without persistent handles.
        let parent = tpm
            .create_primary(
                Handle(rh::OWNER),
                &SensitiveCreate::default(),
                &Public::ecc_storage_parent(Alg::SHA256),
                &mut Auth::Password(b""),
            )
            .map_err(io::Error::other)?;
        let result = (|| {
            let blob = match read_private(&root.join(KEY_FILE)) {
                Ok(blob) => blob,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if fs::read_dir(&root)?.any(|entry| {
                        entry
                            .map(|e| e.path().extension().is_some_and(|ext| ext == "secret"))
                            .unwrap_or(true)
                    }) {
                        return Err(invalid(
                            "sealed TPM key is missing; existing ciphertext was not modified",
                        ));
                    }
                    let mut sensitive = SensitiveCreate {
                        data: Buffer::from(random::<32>()?.to_vec()),
                        ..SensitiveCreate::default()
                    };
                    let created = tpm.create(
                        parent.handle,
                        parent.name.as_slice(),
                        &sensitive,
                        &Public::sealed_data(Alg::SHA256),
                        &mut Auth::Password(b""),
                    );
                    sensitive.data.0.zeroize();
                    let created = created.map_err(io::Error::other)?;
                    let mut blob = KEY_HEADER.to_vec();
                    created.private.marshal(&mut blob);
                    created.public.marshal(&mut blob);
                    atomic_write(&root, KEY_FILE, &blob)?;
                    blob
                }
                Err(error) => return Err(error),
            };
            let data = blob
                .strip_prefix(KEY_HEADER)
                .ok_or_else(|| invalid("unsupported TPM key format"))?;
            let mut reader = Reader::new(data);
            let private =
                Buffer::unmarshal(&mut reader).map_err(|_| invalid("damaged sealed TPM key"))?;
            let public =
                Buffer::unmarshal(&mut reader).map_err(|_| invalid("damaged sealed TPM key"))?;
            if !reader.is_empty() {
                return Err(invalid("trailing data in sealed TPM key"));
            }
            let object = tpm
                .load(
                    parent.handle,
                    parent.name.as_slice(),
                    &private,
                    &public,
                    &mut Auth::Password(b""),
                )
                .map_err(|error| {
                    invalid(format!("cannot load existing sealed TPM key: {error}"))
                })?;
            let result = (|| {
                let mut session = tpm
                    .start_hmac_session(Alg::SHA256)
                    .map_err(io::Error::other)?;
                let result = tpm
                    .unseal(
                        object.handle,
                        object.name.as_slice(),
                        &mut Auth::Session {
                            session: &mut session,
                            auth_value: b"",
                        },
                    )
                    .map(Zeroizing::new)
                    .map_err(|error| invalid(format!("cannot unseal existing TPM key: {error}")));
                let flushed = tpm.flush_context(session.handle).map_err(io::Error::other);
                let key = result?;
                flushed?;
                if key.len() != 32 {
                    return Err(invalid("invalid unsealed TPM key length"));
                }
                Ok(key)
            })();
            let flushed = tpm.flush_context(object.handle).map_err(io::Error::other);
            let key = result?;
            flushed?;
            Ok(key)
        })();
        let flushed = tpm.flush_context(parent.handle).map_err(io::Error::other);
        let key = result?;
        flushed?;
        Ok(Arc::new(Self(Arc::new(Inner { root, key }))))
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(bytes)
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

fn read_private(path: &Path) -> io::Result<Vec<u8>> {
    let file = private_options().read(true).open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "TPM store files must be private regular files (0600)",
        ));
    }
    if meta.len() > MAX_FILE_SIZE {
        return Err(invalid("TPM store file is too large"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_SIZE + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_SIZE {
        return Err(invalid("TPM store file is too large"));
    }
    Ok(bytes)
}

fn atomic_write(root: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let temporary = root.join(format!(".tmp-{:x}", u128::from_le_bytes(random()?)));
    let mut file = private_options()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, root.join(name))?;
        File::open(root)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn secret_filename(identity: &[u8]) -> String {
    let digest: String = Sha256::digest(identity)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{digest}.secret")
}

fn platform(error: io::Error) -> Error {
    if error.kind() == io::ErrorKind::PermissionDenied {
        Error::NoStorageAccess(Box::new(error))
    } else {
        Error::PlatformFailure(Box::new(error))
    }
}

impl CredentialStoreApi for Store {
    fn vendor(&self) -> String {
        "QueryMT TPM 2.0 sealed store".into()
    }
    fn id(&self) -> String {
        self.0.root.display().to_string()
    }
    fn build(
        &self,
        service: &str,
        user: &str,
        modifiers: Option<&HashMap<&str, &str>>,
    ) -> Result<Entry> {
        if modifiers.is_some_and(|mods| !mods.is_empty()) {
            return Err(Error::NotSupportedByStore(
                "TPM store does not support modifiers".into(),
            ));
        }
        let identity = serde_json::to_vec(&(service, user))
            .map_err(|error| platform(io::Error::other(error)))?;
        Ok(Entry::new_with_credential(Arc::new(TpmCredential {
            store: self.0.clone(),
            service: service.to_owned(),
            user: user.to_owned(),
            identity,
        })))
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct TpmCredential {
    store: Arc<Inner>,
    service: String,
    user: String,
    identity: Vec<u8>,
}

impl TpmCredential {
    fn filename(&self) -> String {
        secret_filename(&self.identity)
    }
    fn cipher(&self) -> Aes256Gcm {
        // Store::open validates the fixed key length; it is never serialized.
        Aes256Gcm::new_from_slice(&self.store.key).expect("validated AES-256 key")
    }
}

impl CredentialApi for TpmCredential {
    fn set_secret(&self, secret: &[u8]) -> Result<()> {
        let _lock = lock(&self.store.root).map_err(platform)?;
        let nonce = random::<12>().map_err(platform)?;
        let ciphertext = self
            .cipher()
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: secret,
                    aad: &self.identity,
                },
            )
            .map_err(|_| platform(io::Error::other("credential encryption failed")))?;
        let mut bytes = SECRET_HEADER.to_vec();
        bytes.extend_from_slice(&nonce);
        bytes.extend_from_slice(&ciphertext);
        if bytes.len() as u64 > MAX_FILE_SIZE {
            return Err(platform(invalid("credential is too large")));
        }
        atomic_write(&self.store.root, &self.filename(), &bytes).map_err(platform)
    }
    fn get_secret(&self) -> Result<Vec<u8>> {
        let _lock = lock(&self.store.root).map_err(platform)?;
        let bytes = read_private(&self.store.root.join(self.filename())).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                Error::NoEntry
            } else {
                platform(error)
            }
        })?;
        let data = bytes
            .strip_prefix(SECRET_HEADER)
            .filter(|data| data.len() >= 12 + 16)
            .ok_or_else(|| platform(invalid("damaged encrypted credential")))?;
        self.cipher()
            .decrypt(
                Nonce::from_slice(&data[..12]),
                Payload {
                    msg: &data[12..],
                    aad: &self.identity,
                },
            )
            .map_err(|_| {
                platform(invalid(
                    "credential authentication failed; ciphertext was not modified",
                ))
            })
    }
    fn delete_credential(&self) -> Result<()> {
        let _lock = lock(&self.store.root).map_err(platform)?;
        fs::remove_file(self.store.root.join(self.filename())).map_err(|error| {
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
    use purecrypto_tpm::transport::SimulatorTransport;

    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store(Arc::new(Inner {
            root: dir.path().into(),
            key: Zeroizing::new(vec![7; 32]),
        }));
        (dir, store)
    }

    #[test]
    fn ciphertext_roundtrip_tampering_and_identity_binding() {
        let (dir, store) = fixture();
        let entry = store.build("querymt", "one", None).unwrap();
        entry.set_password("test secret").unwrap();
        assert_eq!(entry.get_password().unwrap(), "test secret");
        let credential = TpmCredential {
            store: store.0.clone(),
            service: "querymt".into(),
            user: "one".into(),
            identity: serde_json::to_vec(&("querymt", "one")).unwrap(),
        };
        let path = dir.path().join(credential.filename());
        let mut bytes = fs::read(&path).unwrap();
        assert!(
            !bytes
                .windows(b"test secret".len())
                .any(|window| window == b"test secret")
        );
        let other_identity = serde_json::to_vec(&("querymt", "two")).unwrap();
        atomic_write(dir.path(), &secret_filename(&other_identity), &bytes).unwrap();
        assert!(
            store
                .build("querymt", "two", None)
                .unwrap()
                .get_password()
                .is_err()
        );
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&path, &bytes).unwrap();
        assert!(entry.get_secret().is_err());
        assert_eq!(fs::read(path).unwrap(), bytes);
        entry.delete_credential().unwrap();
        assert!(matches!(entry.get_secret(), Err(Error::NoEntry)));
    }

    #[test]
    fn refuses_symlinks_and_public_permissions() {
        let (dir, store) = fixture();
        let entry = store.build("querymt", "one", None).unwrap();
        entry.set_password("test").unwrap();
        let identity = serde_json::to_vec(&("querymt", "one")).unwrap();
        let path = dir.path().join(secret_filename(&identity));
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        assert!(entry.get_secret().is_err());
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("elsewhere", &path).unwrap();
        assert!(entry.get_secret().is_err());
    }

    #[test]
    #[ignore = "requires an isolated swtpm at QMT_TEST_TPM_SIM; optional QMT_TEST_TPM_ROOT persists across simulator restarts"]
    fn simulator_seal_reopen_and_corruption() {
        let address = std::env::var("QMT_TEST_TPM_SIM").expect("set QMT_TEST_TPM_SIM");
        let temp = tempfile::tempdir().unwrap();
        let root = std::env::var_os("QMT_TEST_TPM_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| temp.path().join("secrets"));
        let open = || Store::open(root.clone(), SimulatorTransport::connect(&address).unwrap());
        let store = open().expect("create/unseal TPM store");
        let entry = store.build("querymt-test", "restart", None).unwrap();
        // Re-running with the same test directory verifies simulator restart persistence.
        match entry.get_password() {
            Ok(value) => assert_eq!(value, "simulator test secret"),
            Err(Error::NoEntry) => entry.set_password("simulator test secret").unwrap(),
            Err(error) => panic!("cannot read test credential: {error}"),
        }
        drop(entry);
        drop(store);
        assert_eq!(
            open()
                .unwrap()
                .build("querymt-test", "restart", None)
                .unwrap()
                .get_password()
                .unwrap(),
            "simulator test secret"
        );
        if std::env::var_os("QMT_TEST_TPM_ROOT").is_none() {
            let key = root.join(KEY_FILE);
            let original = fs::read(&key).unwrap();
            fs::write(&key, b"damaged").unwrap();
            assert_eq!(open().err().unwrap().kind(), io::ErrorKind::InvalidData);
            assert_eq!(fs::read(&key).unwrap(), b"damaged");
            fs::remove_file(&key).unwrap();
            assert_eq!(open().err().unwrap().kind(), io::ErrorKind::InvalidData);
            assert!(!key.exists());
            fs::write(&key, original).unwrap();
        }
    }
}

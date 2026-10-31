//! Bounded zbus operations. The upstream keyring adapter does not expose its
//! connection or deadlines, so use the same Secret Service client directly.

use std::{any::Any, collections::HashMap, future::Future, io, sync::Arc, time::Duration};

use keyring_core::{
    Entry, Error, Result,
    api::{Credential, CredentialApi, CredentialStoreApi},
};
use secret_service::{EncryptionType, Item, SecretService};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct Store(Arc<SecretService<'static>>);

fn bounded<T>(timeout: Duration, operation: impl Future<Output = Result<T>>) -> Result<T> {
    // Dropping the losing future cancels the actual I/O, including authentication;
    // no detached blocking task can keep the global initialization lock occupied.
    async_io::block_on(futures_lite::future::or(operation, async {
        async_io::Timer::after(timeout).await;
        Err(Error::PlatformFailure(Box::new(io::Error::new(
            io::ErrorKind::TimedOut,
            "Secret Service operation timed out",
        ))))
    }))
}

fn service_error(error: secret_service::Error) -> Error {
    match error {
        secret_service::Error::Locked
        | secret_service::Error::NoResult
        | secret_service::Error::Prompt => Error::NoStorageAccess(Box::new(error)),
        _ => Error::PlatformFailure(Box::new(error)),
    }
}

impl Store {
    pub(super) fn new() -> io::Result<Arc<Self>> {
        let builder = zbus::connection::Builder::session().map_err(io::Error::other)?;
        Self::connect(builder, CONNECT_TIMEOUT).map_err(super::super::map_keyring_error)
    }

    fn connect(builder: zbus::connection::Builder<'_>, timeout: Duration) -> Result<Arc<Self>> {
        bounded(timeout, async {
            let connection = builder
                .method_timeout(OPERATION_TIMEOUT)
                .build()
                .await
                .map_err(|error| Error::PlatformFailure(Box::new(error)))?;
            let service = SecretService::connect_with_existing(EncryptionType::Dh, connection)
                .await
                .map_err(service_error)?;
            // Existing items are searched globally. Only creation needs the
            // default collection; absence of its alias must not hide saved keys.
            Ok(Arc::new(Self(Arc::new(service))))
        })
    }
}

impl CredentialStoreApi for Store {
    fn vendor(&self) -> String {
        "QueryMT zbus Secret Service".into()
    }
    fn id(&self) -> String {
        "secret-service".into()
    }
    fn build(
        &self,
        service: &str,
        user: &str,
        modifiers: Option<&HashMap<&str, &str>>,
    ) -> Result<Entry> {
        if modifiers.is_some_and(|mods| !mods.is_empty()) {
            return Err(Error::NotSupportedByStore(
                "Secret Service modifiers are not supported".into(),
            ));
        }
        Ok(Entry::new_with_credential(Arc::new(ServiceCredential {
            connection: self.0.clone(),
            service: service.to_owned(),
            user: user.to_owned(),
        })))
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct ServiceCredential {
    connection: Arc<SecretService<'static>>,
    service: String,
    user: String,
}

impl ServiceCredential {
    fn attributes(&self) -> HashMap<&str, &str> {
        HashMap::from([
            ("service", self.service.as_str()),
            ("username", self.user.as_str()),
        ])
    }

    async fn existing(&self) -> Result<Item<'_>> {
        let matches = self
            .connection
            .search_items(self.attributes())
            .await
            .map_err(service_error)?;
        let mut items = matches.unlocked.into_iter().chain(matches.locked);
        let item = items.next().ok_or(Error::NoEntry)?;
        if items.next().is_some() {
            return Err(Error::PlatformFailure(Box::new(io::Error::other(
                "multiple Secret Service entries match this credential",
            ))));
        }
        if item.is_locked().await.map_err(service_error)? {
            item.unlock().await.map_err(service_error)?;
        }
        Ok(item)
    }
}

impl CredentialApi for ServiceCredential {
    fn set_secret(&self, secret: &[u8]) -> Result<()> {
        bounded(OPERATION_TIMEOUT, async {
            match self.existing().await {
                Ok(item) => item
                    .set_secret(secret, "application/octet-stream")
                    .await
                    .map_err(service_error),
                Err(Error::NoEntry) => {
                    let collection = self
                        .connection
                        .get_default_collection()
                        .await
                        .map_err(service_error)?;
                    if collection.is_locked().await.map_err(service_error)? {
                        collection.unlock().await.map_err(service_error)?;
                    }
                    collection
                        .create_item(
                            &format!("keyring:{}@{}", self.user, self.service),
                            self.attributes(),
                            secret,
                            true,
                            "application/octet-stream",
                        )
                        .await
                        .map_err(service_error)?;
                    Ok(())
                }
                Err(error) => Err(error),
            }
        })
    }
    fn get_secret(&self) -> Result<Vec<u8>> {
        bounded(OPERATION_TIMEOUT, async {
            self.existing()
                .await?
                .get_secret()
                .await
                .map_err(service_error)
        })
    }
    fn delete_credential(&self) -> Result<()> {
        bounded(OPERATION_TIMEOUT, async {
            self.existing().await?.delete().await.map_err(service_error)
        })
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

    #[test]
    fn stalled_bus_authentication_is_cancelled_at_the_socket() {
        use std::{io::Read, os::unix::net::UnixListener};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bus");
        let listener = UnixListener::bind(&path).unwrap();
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = [0; 1024];
            // Authentication data may arrive first. Cancellation must close it.
            loop {
                if stream.read(&mut bytes).unwrap() == 0 {
                    break;
                }
            }
        });
        let address = format!("unix:path={}", path.display());
        let start = std::time::Instant::now();
        let error = Store::connect(
            zbus::connection::Builder::address(address.as_str()).unwrap(),
            Duration::from_millis(100),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(2));
        peer.join().unwrap();
    }

    #[test]
    #[ignore = "requires a disposable Secret Service with no default alias and an unlocked session collection"]
    fn existing_items_work_without_a_default_collection() {
        assert_eq!(
            std::env::var("QMT_TEST_ISOLATED_KEYRING").as_deref(),
            Ok("1")
        );
        let store = Store::new().unwrap();
        bounded(OPERATION_TIMEOUT, async {
            assert!(matches!(
                store.0.get_default_collection().await,
                Err(secret_service::Error::NoResult)
            ));
            let session = store
                .0
                .get_collection_by_alias("session")
                .await
                .map_err(service_error)?;
            session
                .create_item(
                    "QueryMT regression fixture",
                    HashMap::from([("service", "querymt-test"), ("username", "no-default")]),
                    b"initial",
                    true,
                    "text/plain",
                )
                .await
                .map_err(service_error)?;
            Ok(())
        })
        .unwrap();
        let entry = store.build("querymt-test", "no-default", None).unwrap();
        assert_eq!(entry.get_password().unwrap(), "initial");
        entry.set_password("updated").unwrap();
        assert_eq!(entry.get_password().unwrap(), "updated");
        entry.delete_credential().unwrap();
        assert!(matches!(entry.get_secret(), Err(Error::NoEntry)));
    }
}

//! Linux backend selection. Preferences contain backend names, never credentials.

use std::{io, path::PathBuf, sync::Arc};

use keyring_core::{CredentialStore, api::CredentialStoreApi};

#[cfg(feature = "tpm")]
mod tpm;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    SecretService,
    Tpm,
    Keyutils,
}

fn parse_preference(value: &str) -> io::Result<Option<Backend>> {
    match value.trim() {
        "auto" => Ok(None),
        "secret-service" => Ok(Some(Backend::SecretService)),
        "tpm" => Ok(Some(Backend::Tpm)),
        "keyutils" => Ok(Some(Backend::Keyutils)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected auto, secret-service, tpm, or keyutils",
        )),
    }
}

fn config_dir() -> io::Result<PathBuf> {
    for name in ["QMT_CONFIG_DIR", "QMT_HOME"] {
        if let Ok(path) = std::env::var(name)
            && !path.trim().is_empty()
        {
            return Ok(path.into());
        }
    }
    dirs::home_dir()
        .map(|home| home.join(".qmt"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory found"))
}

fn preference() -> io::Result<Option<Backend>> {
    if let Some(value) = std::env::var_os("QMT_SECRET_STORE") {
        if let Some(value) = value.to_str() {
            match parse_preference(value) {
                Ok(backend) => {
                    log::info!("Credential store override: QMT_SECRET_STORE={value}");
                    return Ok(backend);
                }
                Err(error) => log::warn!("Ignoring invalid QMT_SECRET_STORE: {error}"),
            }
        } else {
            log::warn!("Ignoring non-Unicode QMT_SECRET_STORE");
        }
    }
    let path = config_dir()?.join("secret-store");
    match std::fs::read_to_string(&path) {
        Ok(value) => {
            let backend = parse_preference(&value).map_err(|error| {
                io::Error::new(error.kind(), format!("{}: {error}", path.display()))
            })?;
            log::info!(
                "Credential store preference from {}: {}",
                path.display(),
                value.trim()
            );
            Ok(backend)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn select<T>(
    preferred: Option<Backend>,
    mut probe: impl FnMut(Backend) -> io::Result<T>,
) -> io::Result<(Backend, T)> {
    if let Some(backend) = preferred {
        return probe(backend).map(|store| (backend, store));
    }
    let mut failures = Vec::new();
    for backend in [Backend::SecretService, Backend::Tpm, Backend::Keyutils] {
        match probe(backend) {
            Ok(store) => return Ok((backend, store)),
            // Damaged persistent storage must not be mistaken for an empty store.
            Err(error) if error.kind() == io::ErrorKind::InvalidData => return Err(error),
            Err(error) => {
                if backend == Backend::Tpm {
                    log::warn!("TPM credential storage is unavailable: {error}");
                } else {
                    log::debug!("{backend:?} credential storage unavailable: {error}");
                }
                failures.push(format!("{backend:?}: {error}"));
            }
        }
    }
    Err(io::Error::other(format!(
        "No usable credential store: {}",
        failures.join("; ")
    )))
}

fn probe(backend: Backend) -> io::Result<Arc<CredentialStore>> {
    match backend {
        Backend::SecretService => {
            // Verify the default collection exists without prompting or writing.
            // Keep locked wallets on this backend; credential calls can unlock
            // them on a blocking worker instead of hiding their existing keys.
            let service = secret_service::blocking::SecretService::connect(
                secret_service::EncryptionType::Dh,
            )
            .map_err(io::Error::other)?;
            service.get_default_collection().map_err(io::Error::other)?;
            zbus_secret_service_keyring_store::Store::new()
                .map(|store| store as Arc<CredentialStore>)
                .map_err(super::map_keyring_error)
        }
        Backend::Tpm => {
            #[cfg(feature = "tpm")]
            {
                tpm::Store::new(config_dir()?.join("tpm-secrets"))
                    .map(|store| store as Arc<CredentialStore>)
            }
            #[cfg(not(feature = "tpm"))]
            {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "TPM support was disabled at build time",
                ))
            }
        }
        Backend::Keyutils => {
            let store =
                linux_keyutils_keyring_store::Store::new().map_err(super::map_keyring_error)?;
            // Store::new() is infallible; exercise the kernel rather than assuming
            // keyring syscalls are permitted (containers may deny them).
            let name = format!(
                "probe-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(io::Error::other)?
                    .as_nanos()
            );
            let entry = store
                .build("querymt-probe", &name, None)
                .map_err(super::map_keyring_error)?;
            entry
                .set_password("probe")
                .map_err(super::map_keyring_error)?;
            let read = entry.get_password().map_err(super::map_keyring_error);
            let deleted = entry.delete_credential().map_err(super::map_keyring_error);
            if read? != "probe" {
                return Err(io::Error::other("keyutils probe readback failed"));
            }
            deleted?;
            Ok(store)
        }
    }
}

pub(super) fn init() -> io::Result<()> {
    let (backend, store) = select(preference()?, probe)?;
    keyring_core::set_default_store(store);
    match backend {
        Backend::SecretService => {
            log::info!("Using D-Bus Secret Service for credential storage (zbus)")
        }
        Backend::Tpm => log::info!("Using TPM 2.0 sealed storage for credentials"),
        Backend::Keyutils => log::warn!(
            "Using temporary kernel keyutils credential storage; secrets will be lost on reboot and may be lost earlier if the keyring expires or is cleared"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_in_priority_order_and_stops_at_success() {
        for available in [Backend::SecretService, Backend::Tpm, Backend::Keyutils] {
            let mut attempted = Vec::new();
            let (chosen, ()) = select(None, |backend| {
                attempted.push(backend);
                if backend == available {
                    Ok(())
                } else {
                    Err(io::ErrorKind::NotFound.into())
                }
            })
            .unwrap();
            assert_eq!(chosen, available);
            assert_eq!(
                attempted,
                [Backend::SecretService, Backend::Tpm, Backend::Keyutils]
                    .into_iter()
                    .take(attempted.len())
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn explicit_backend_does_not_fall_back() {
        let mut attempted = Vec::new();
        let error = select(Some(Backend::Tpm), |backend| -> io::Result<()> {
            attempted.push(backend);
            Err(io::ErrorKind::PermissionDenied.into())
        })
        .unwrap_err();
        assert_eq!(attempted, [Backend::Tpm]);
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn corruption_and_total_unavailability_are_errors() {
        let mut attempted = Vec::new();
        let error = select(None, |backend| -> io::Result<()> {
            attempted.push(backend);
            Err(if backend == Backend::Tpm {
                io::ErrorKind::InvalidData
            } else {
                io::ErrorKind::NotFound
            }
            .into())
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(attempted, [Backend::SecretService, Backend::Tpm]);
        assert!(
            select(None, |_| -> io::Result<()> {
                Err(io::ErrorKind::PermissionDenied.into())
            })
            .is_err()
        );
    }

    #[test]
    fn preferences_accept_only_documented_backends() {
        assert_eq!(parse_preference("auto\n").unwrap(), None);
        assert_eq!(
            parse_preference("secret-service").unwrap(),
            Some(Backend::SecretService)
        );
        assert_eq!(parse_preference("tpm").unwrap(), Some(Backend::Tpm));
        assert_eq!(
            parse_preference("keyutils").unwrap(),
            Some(Backend::Keyutils)
        );
        assert!(parse_preference("file").is_err());
    }
}

//! Persistent mesh identity management.
//!
//! Each mesh node needs a stable ed25519 keypair so that its `PeerId` persists
//! across process restarts.  Without this, invite tokens (which contain a
//! `PeerId`) become invalid every time the process is restarted.
//!
//! The keypair is stored as a raw 64-byte ed25519 secret key file at
//! `~/.qmt/mesh_identity.key` (or a custom path via config).  The file is
//! created with `0o600` permissions on Unix.

use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};

/// Default filename within the qmt config directory.
const DEFAULT_FILENAME: &str = "mesh_identity.key";

/// Return the default identity file path: `~/.qmt/mesh_identity.key`.
pub fn default_identity_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine home directory"))?;
    Ok(home.join(".qmt").join(DEFAULT_FILENAME))
}

/// Load an existing keypair from `path`, or generate a new one and persist it.
///
/// If `path` is `None`, the default path (`~/.qmt/mesh_identity.key`) is used.
///
/// # File format
///
/// Raw 64-byte ed25519 secret key (the "expanded" form that libp2p uses).
/// No header, no framing — just the bytes.
///
/// # Permissions
///
/// On Unix the file is created with mode `0o600` (owner read/write only).
#[cfg(feature = "remote")]
pub fn load_or_generate_keypair(path: Option<&Path>) -> Result<libp2p::identity::Keypair> {
    let path = match path {
        Some(p) => p.to_path_buf(),
        None => default_identity_path()?,
    };

    if path.exists() {
        load_keypair(&path)
    } else {
        let keypair = generate_and_save_keypair(&path)?;
        log::info!(
            "Generated new mesh identity: peer_id={}, saved to {}",
            keypair.public().to_peer_id(),
            path.display()
        );
        Ok(keypair)
    }
}

/// Load a keypair from an existing file.
#[cfg(feature = "remote")]
fn load_keypair(path: &Path) -> Result<libp2p::identity::Keypair> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read mesh identity from {}", path.display()))?;

    if bytes.len() != 64 {
        anyhow::bail!(
            "mesh identity file {} has {} bytes, expected 64",
            path.display(),
            bytes.len()
        );
    }

    let ed25519_keypair = libp2p::identity::ed25519::Keypair::try_from_bytes(&mut bytes.clone())
        .with_context(|| format!("failed to parse ed25519 keypair from {}", path.display()))?;

    let keypair: libp2p::identity::Keypair = ed25519_keypair.into();
    log::info!(
        "Loaded mesh identity: peer_id={}, from {}",
        keypair.public().to_peer_id(),
        path.display()
    );
    Ok(keypair)
}

/// Generate a new ed25519 keypair, write it to `path`, and return it.
#[cfg(feature = "remote")]
fn generate_and_save_keypair(path: &Path) -> Result<libp2p::identity::Keypair> {
    // Ensure parent directory exists.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create directory {} for mesh identity",
                parent.display()
            )
        })?;
    }

    let keypair = libp2p::identity::Keypair::generate_ed25519();
    let ed25519 = keypair
        .clone()
        .try_into_ed25519()
        .map_err(|e| anyhow!("keypair is not ed25519: {}", e))?;

    // ed25519::Keypair::to_bytes() returns 64 bytes: 32-byte secret + 32-byte public.
    let bytes = ed25519.to_bytes();

    std::fs::write(path, bytes)
        .with_context(|| format!("failed to write mesh identity to {}", path.display()))?;

    // Set file permissions to 0o600 on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }

    Ok(keypair)
}

#[cfg(all(test, feature = "remote"))]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_generate_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_identity.key");

        // Generate
        let kp1 = generate_and_save_keypair(&path).unwrap();
        assert!(path.exists());

        // File is 64 bytes
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 64);

        // Load
        let kp2 = load_keypair(&path).unwrap();

        // Same PeerId
        assert_eq!(kp1.public().to_peer_id(), kp2.public().to_peer_id());
    }

    #[test]
    fn load_or_generate_creates_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("identity.key");
        assert!(!path.exists());

        let kp = load_or_generate_keypair(Some(&path)).unwrap();
        assert!(path.exists());

        // Calling again loads the same identity
        let kp2 = load_or_generate_keypair(Some(&path)).unwrap();
        assert_eq!(kp.public().to_peer_id(), kp2.public().to_peer_id());
    }

    #[cfg(unix)]
    #[test]
    fn file_permissions_are_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");

        generate_and_save_keypair(&path).unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0o600, got 0o{:o}", mode);
    }

    #[test]
    fn rejects_wrong_size_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad_identity.key");
        std::fs::write(&path, b"too short").unwrap();

        let result = load_keypair(&path);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("expected 64"),
            "error should mention expected size"
        );
    }
}

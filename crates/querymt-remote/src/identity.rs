//! Persistent mesh identity management.

use anyhow::Result;
#[cfg(feature = "kameo-mesh")]
use anyhow::{Context, anyhow};
use std::path::PathBuf;

const DEFAULT_FILENAME: &str = "mesh_identity.key";

pub fn default_identity_path() -> Result<PathBuf> {
    let cfg_dir = querymt_utils::providers::config_dir()?;
    Ok(cfg_dir.join(DEFAULT_FILENAME))
}

#[cfg(feature = "kameo-mesh")]
pub fn load_or_generate_keypair(path: Option<&std::path::Path>) -> Result<libp2p::identity::Keypair> {
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

#[cfg(feature = "kameo-mesh")]
fn load_keypair(path: &std::path::Path) -> Result<libp2p::identity::Keypair> {
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

#[cfg(feature = "kameo-mesh")]
fn generate_and_save_keypair(path: &std::path::Path) -> Result<libp2p::identity::Keypair> {
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

    let bytes = ed25519.to_bytes();

    std::fs::write(path, bytes)
        .with_context(|| format!("failed to write mesh identity to {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }

    Ok(keypair)
}

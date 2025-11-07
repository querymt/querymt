use crate::error::LLMError;
use anyhow::anyhow;
use docker_credential::{CredentialRetrievalError, DockerCredential};
use oci_client::{
    errors::{OciDistributionError, OciErrorCode},
    manifest::{OciImageManifest, OciManifest, Platform},
    secrets::RegistryAuth,
    Client, Reference,
};
use serde::{Deserialize, Serialize};
use sigstore::cosign::verification_constraint::cert_subject_email_verifier::StringVerifier;
use sigstore::cosign::verification_constraint::{
    CertSubjectEmailVerifier, CertSubjectUrlVerifier, VerificationConstraintVec,
};
use sigstore::cosign::{verify_constraints, ClientBuilder, CosignCapabilities};
use sigstore::errors::SigstoreVerifyConstraintsError;
use sigstore::registry::{Auth, OciReference};
use sigstore::trust::sigstore::SigstoreTrustRoot;
use sigstore::trust::{ManualTrustRoot, TrustRoot};
use std::env::consts::{ARCH, OS};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::instrument;

use super::{PluginType, ProviderPlugin};

const PLUGIN_TYPE_ANNOTATION: &str = "mt.query.plugin.type";

#[derive(Debug, Serialize, Deserialize, Clone)]
struct CacheMetadata {
    /// The immutable digest the tag pointed to, e.g., "sha256:..."
    manifest_digest: String,
    /// The discovered filename, e.g., "plugin.wasm"
    filename: String,
    /// The discovered plugin type, e.g., "native" or "extism"
    plugin_type_str: String,
    /// When this metadata was last updated
    retrieved_at_unix: u64,
}

// Docker manifest format v2
#[derive(Debug, Serialize, Deserialize)]
struct DockerManifest {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    #[serde(rename = "mediaType")]
    media_type: String,
    config: DockerManifestConfig,
    layers: Vec<DockerManifestLayer>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DockerManifestConfig {
    #[serde(rename = "mediaType")]
    media_type: String,
    size: u64,
    digest: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct DockerManifestLayer {
    #[serde(rename = "mediaType")]
    media_type: String,
    size: u64,
    digest: String,
}

fn build_auth(reference: &Reference) -> RegistryAuth {
    let server = reference
        .resolve_registry()
        .strip_suffix('/')
        .unwrap_or_else(|| reference.resolve_registry());

    match docker_credential::get_credential(server) {
        Err(CredentialRetrievalError::ConfigNotFound) => RegistryAuth::Anonymous,
        Err(CredentialRetrievalError::NoCredentialConfigured) => RegistryAuth::Anonymous,
        Err(e) => {
            log::info!(
                "Error retrieving docker credentials: {}. Using anonymous auth",
                e
            );
            RegistryAuth::Anonymous
        }
        Ok(DockerCredential::UsernamePassword(username, password)) => {
            log::info!("Found docker credentials");
            RegistryAuth::Basic(username, password)
        }
        Ok(DockerCredential::IdentityToken(_)) => {
            log::info!(
                "Cannot use contents of docker config, identity token not supported. Using anonymous auth"
            );
            RegistryAuth::Anonymous
        }
    }
}

async fn setup_trust_repository(
    config: &OciDownloaderConfig,
) -> Result<Box<dyn TrustRoot>, anyhow::Error> {
    if config.use_sigstore_tuf_data {
        // Use Sigstore TUF data from the official repository
        log::info!("Using Sigstore TUF data for verification");
        match SigstoreTrustRoot::new(None).await {
            Ok(repo) => return Ok(Box::new(repo)),
            Err(e) => {
                log::warn!("Failed to initialize TUF trust repository: {}", e);
                log::info!("Falling back to manual trust repository");
            }
        }
    }

    // Create a manual trust repository
    let mut data = ManualTrustRoot::default();

    // Add Rekor public keys if provided
    if let Some(rekor_keys_path) = &config.rekor_pub_keys {
        if rekor_keys_path.exists() {
            match fs::read(rekor_keys_path) {
                Ok(content) => {
                    log::info!("Added Rekor public key");
                    data.rekor_keys.push(content);
                }
                Err(e) => log::warn!("Failed to read Rekor public keys file: {}", e),
            }
        } else {
            log::warn!("Rekor public keys file not found: {:?}", rekor_keys_path);
        }
    }

    // Add Fulcio certificates if provided
    if let Some(fulcio_certs_path) = &config.fulcio_certs {
        if fulcio_certs_path.exists() {
            match fs::read(fulcio_certs_path) {
                Ok(content) => {
                    let certificate = sigstore::registry::Certificate {
                        encoding: sigstore::registry::CertificateEncoding::Pem,
                        data: content,
                    };

                    match certificate.try_into() {
                        Ok(cert) => {
                            log::info!("Added Fulcio certificate");
                            data.fulcio_certs.push(cert);
                        }
                        Err(e) => log::warn!("Failed to parse Fulcio certificate: {}", e),
                    }
                }
                Err(e) => log::warn!("Failed to read Fulcio certificates file: {}", e),
            }
        } else {
            log::warn!(
                "Fulcio certificates file not found: {:?}",
                fulcio_certs_path
            );
        }
    }

    Ok(Box::new(data))
}

#[instrument(name = "oci.verify_image_signature", skip_all, fields(image = %image_reference))]
async fn verify_image_signature(
    config: &OciDownloaderConfig,
    image_reference: &str,
) -> Result<bool, anyhow::Error> {
    log::info!("Verifying signature for {}", image_reference);

    // Set up the trust repository based on CLI arguments
    let repo = setup_trust_repository(config).await?;
    let auth = &Auth::Anonymous;

    // Create a client builder
    let client_builder = ClientBuilder::default();

    // Create client with trust repository
    let client_builder = match client_builder.with_trust_repository(repo.as_ref()) {
        Ok(builder) => builder,
        Err(e) => return Err(anyhow!("Failed to set up trust repository: {}", e)),
    };

    // Build the client
    let mut client = match client_builder.build() {
        Ok(client) => client,
        Err(e) => return Err(anyhow!("Failed to build Sigstore client: {}", e)),
    };

    // Parse the reference
    let image_ref = match OciReference::from_str(image_reference) {
        Ok(reference) => reference,
        Err(e) => return Err(anyhow!("Invalid image reference: {}", e)),
    };

    // Triangulate to find the signature image and source digest
    let (cosign_signature_image, source_image_digest) =
        match client.triangulate(&image_ref, auth).await {
            Ok((sig_image, digest)) => (sig_image, digest),
            Err(e) => {
                log::warn!("Failed to triangulate image: {}", e);
                return Ok(false); // No signatures found
            }
        };

    // Get trusted signature layers
    let signature_layers = match client
        .trusted_signature_layers(auth, &source_image_digest, &cosign_signature_image)
        .await
    {
        Ok(layers) => layers,
        Err(e) => {
            log::warn!("Failed to get trusted signature layers: {}", e);
            return Ok(false);
        }
    };

    if signature_layers.is_empty() {
        log::warn!("No valid signatures found for {}", image_reference);
        return Ok(false);
    }

    // Build verification constraints based on CLI options
    let mut verification_constraints: VerificationConstraintVec = Vec::new();

    if let Some(cert_email) = &config.cert_email {
        let issuer = config
            .cert_issuer
            .as_ref()
            .map(|i| StringVerifier::ExactMatch(i.to_string()));

        verification_constraints.push(Box::new(CertSubjectEmailVerifier {
            email: StringVerifier::ExactMatch(cert_email.to_string()),
            issuer,
        }));
    }

    if let Some(cert_url) = &config.cert_url {
        match config.cert_issuer.as_ref() {
            Some(issuer) => {
                verification_constraints.push(Box::new(CertSubjectUrlVerifier {
                    url: cert_url.to_string(),
                    issuer: issuer.to_string(),
                }));
            }
            None => {
                log::warn!("'cert-issuer' is required when 'cert-url' is specified");
            }
        }
    }

    // Verify the constraints
    match verify_constraints(&signature_layers, verification_constraints.iter()) {
        Ok(()) => {
            log::info!("Signature verification successful for {}", image_reference);
            Ok(true)
        }
        Err(SigstoreVerifyConstraintsError {
            unsatisfied_constraints,
        }) => {
            log::warn!(
                "Signature verification failed for {}: {:?}",
                image_reference,
                unsatisfied_constraints
            );
            Ok(false)
        }
    }
}

async fn extract_file_and_content(
    client: &Client,
    reference: &Reference,
    image_manifest: &OciImageManifest,
    plugin_type: PluginType,
    filename: Option<&str>,
) -> Result<(String, Vec<u8>), Box<dyn std::error::Error>> {
    match plugin_type {
        PluginType::Wasm => {
            // Find the wasm layer and extract it.
            for layer in &image_manifest.layers {
                if layer.media_type == "application/vnd.wasm.v1.layer+wasm" {
                    let mut wasm_bytes = Vec::new();
                    client.pull_blob(reference, layer, &mut wasm_bytes).await?;
                    let filename = filename.unwrap_or("plugin.wasm").to_string();
                    return Ok((filename, wasm_bytes));
                }
            }
            Err("Wasm plugin type was determined, but no Wasm layer was found.".into())
        }
        PluginType::Native => {
            // Find the native tarball layer and extract the file from it.
            for layer in &image_manifest.layers {
                if layer.media_type == "application/vnd.oci.image.layer.v1.tar+gzip" {
                    let mut buf = Vec::new();
                    client.pull_blob(reference, layer, &mut buf).await?;
                    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&buf[..]));

                    for entry_result in archive.entries()? {
                        let mut entry = entry_result?;
                        if entry.header().entry_type().is_file() {
                            let path = entry.path()?.to_string_lossy().to_string();
                            let current_filename = Path::new(&path)
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy();

                            let matches = filename.is_none_or(|target| current_filename == target);

                            if matches {
                                let mut content = Vec::new();
                                entry.read_to_end(&mut content)?;
                                return Ok((current_filename.to_string(), content));
                            }
                        }
                    }
                }
            }
            Err("Native plugin type was determined, but no .tar.gzip layer was found.".into())
        }
    }
}

fn get_blob_path(cache_root: &Path, digest: &str, filename: &str) -> PathBuf {
    let sanitized_digest = digest.replace(':', "_");
    cache_root
        .join("blobs")
        .join(sanitized_digest)
        .join(filename)
}

fn load_from_cache(
    meta: &CacheMetadata,
    blob_path: &Path,
) -> Result<ProviderPlugin, Box<dyn std::error::Error>> {
    let plugin_type = match meta.plugin_type_str.as_str() {
        "extism" => PluginType::Wasm,
        "native" => PluginType::Native,
        _ => return Err("Invalid plugin type in cache metadata".into()),
    };

    Ok(ProviderPlugin {
        plugin_type,
        file_path: blob_path.to_path_buf(),
    })
}

/// The heuristic logic for determining plugin type from a manifest.
fn determine_plugin_type(image_manifest: &OciImageManifest) -> Result<PluginType, LLMError> {
    for layer in &image_manifest.layers {
        if layer.media_type == "application/vnd.wasm.v1.layer+wasm" {
            return Ok(PluginType::Wasm);
        }
    }

    if let Some(annotations) = &image_manifest.annotations {
        if let Some(plugin_type_str) = annotations.get(PLUGIN_TYPE_ANNOTATION) {
            return match plugin_type_str.as_str() {
                "extism" => Ok(PluginType::Wasm),
                "native" => Ok(PluginType::Native),
                _ => todo!(),
            };
        }
    }

    for layer in &image_manifest.layers {
        if layer.media_type == "application/vnd.oci.image.layer.v1.tar+gzip" {
            return Ok(PluginType::Native);
        }
    }

    Err(LLMError::PluginError(
        "Could not determine plugin type from manifest layers or annotations.".into(),
    ))
}

#[derive(Default, Deserialize, Debug, Clone)]
pub struct OciDownloaderConfig {
    insecure_skip_signature: bool,
    cert_email: Option<String>,
    cert_issuer: Option<String>,
    cert_url: Option<String>,
    use_sigstore_tuf_data: bool,
    rekor_pub_keys: Option<PathBuf>,
    fulcio_certs: Option<PathBuf>,
}

pub struct OciDownloader {
    config: OciDownloaderConfig,
}

impl OciDownloader {
    pub fn new(config: Option<OciDownloaderConfig>) -> Self {
        Self {
            config: config.unwrap_or_default(),
        }
    }

    #[instrument(name = "oci.pull_and_extract", skip_all, fields(image = %image_reference))]
    pub async fn pull_and_extract(
        &self,
        image_reference: &str,
        target_file_path: Option<&str>,
        cache_path: &Path,
        force_update: bool,
    ) -> Result<ProviderPlugin, Box<dyn std::error::Error>> {
        let sanitized_tag_path = image_reference.replace(['/', ':'], "_");
        let manifests_cache_dir = cache_path.join("manifests");
        fs::create_dir_all(&manifests_cache_dir)?;
        let metadata_path = manifests_cache_dir.join(format!("{}.json", sanitized_tag_path));

        let local_metadata: Option<CacheMetadata> = fs::read(&metadata_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());

        if !force_update {
            if let Some(meta) = &local_metadata {
                let blob_path = get_blob_path(cache_path, &meta.manifest_digest, &meta.filename);
                if blob_path.exists() {
                    log::debug!("Found cached OCI plugin. Using local version.");
                    return load_from_cache(meta, &blob_path);
                }
            }
        }

        log::info!("Pulling {} ...", image_reference);

        let client_config = oci_client::client::ClientConfig::default();
        let client = Client::new(client_config);

        let reference = Reference::try_from(image_reference)?;
        let auth = build_auth(&reference);

        // Verify the image signature if it's an OCI image and verification is enabled
        if self.config.insecure_skip_signature {
            log::info!("Signature verification enabled for {}", image_reference);
            match verify_image_signature(&self.config, image_reference).await {
                Ok(verified) => {
                    if !verified {
                        return Err(format!(
                            "No valid signatures found for the image {}",
                            image_reference
                        )
                        .into());
                    }
                }
                Err(e) => {
                    return Err(format!("Image signature verification failed: {}", e).into());
                }
            }
        } else {
            log::warn!("Signature verification disabled for {}", image_reference);
        }

        match client.pull_manifest(&reference, &auth).await {
            Ok((live_manifest, live_digest)) => {
                if let Some(meta) = &local_metadata {
                    let blob_path =
                        get_blob_path(cache_path, &meta.manifest_digest, &meta.filename);
                    if meta.manifest_digest == live_digest && blob_path.exists() {
                        log::debug!("Local cache is up-to-date.");
                        return load_from_cache(meta, &blob_path);
                    }
                }

                let image_manifest;
                let discovered_type;
                match live_manifest {
                    OciManifest::Image(img) => {
                        log::debug!("Found a single image manifest.");
                        discovered_type = determine_plugin_type(&img)?;
                        image_manifest = img;
                    }
                    OciManifest::ImageIndex(index) => {
                        log::debug!("Found a multi-platform image index.");

                        let native_platform = Platform {
                            os: OS.into(),
                            architecture: ARCH.into(),
                            os_version: None,
                            os_features: None,
                            variant: None,
                            features: None,
                        };
                        log::debug!(
                            "Searching for platform: {}/{}",
                            native_platform.os,
                            native_platform.architecture
                        );

                        let maybe_descriptor = index
                            .manifests
                            .iter()
                            .find(|m| m.platform.as_ref() == Some(&native_platform));

                        let manifest_descriptor;

                        if let Some(descriptor) = maybe_descriptor {
                            log::debug!(
                                "Native version found. Using digest: {}",
                                descriptor.digest
                            );
                            manifest_descriptor = descriptor;
                            discovered_type = PluginType::Native;
                        } else {
                            log::debug!(
                                "Native version not found. Checking for wasi/wasm fallback..."
                            );

                            let wasm_platform = Platform {
                                os: "wasi".to_string(),
                                architecture: "wasm".to_string(),
                                os_version: None,
                                os_features: None,
                                variant: None,
                                features: None,
                            };

                            let maybe_wasm_descriptor = index
                                .manifests
                                .iter()
                                .find(|m| m.platform.as_ref() == Some(&wasm_platform));

                            if let Some(descriptor) = maybe_wasm_descriptor {
                                log::debug!(
                                    "Wasm fallback found. Using digest: {}",
                                    descriptor.digest
                                );
                                manifest_descriptor = descriptor;
                                discovered_type = PluginType::Wasm;
                            } else {
                                // --- Failure Case: Neither native nor Wasm was found ---
                                return Err(format!("Image index contains no manifest for the host platform ({}/{}) and no wasi/wasm fallback was found.",
                                    OS, ARCH
                                ).into());
                            }
                        }

                        let manifest_reference =
                            reference.clone_with_digest(manifest_descriptor.digest.clone());

                        let (platform_manifest, _) =
                            client.pull_manifest(&manifest_reference, &auth).await?;
                        if let OciManifest::Image(img) = platform_manifest {
                            image_manifest = img;
                        } else {
                            return Err("Expected an image manifest for the specified platform, but got something else.".into());
                        }
                    }
                }

                let (filename, content) = extract_file_and_content(
                    &client,
                    &reference,
                    &image_manifest,
                    discovered_type,
                    target_file_path,
                )
                .await?;

                let blob_path = get_blob_path(cache_path, &live_digest, &filename);
                fs::create_dir_all(blob_path.parent().unwrap())?;
                fs::write(&blob_path, &content)?;
                log::debug!("Populated OCI blob cache at: {}", blob_path.display());

                let new_metadata = CacheMetadata {
                    manifest_digest: live_digest.to_string(),
                    filename: filename.clone(),
                    plugin_type_str: match discovered_type {
                        PluginType::Wasm => "extism".to_string(),
                        PluginType::Native => "native".to_string(),
                    },
                    retrieved_at_unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
                };
                fs::write(metadata_path, serde_json::to_vec(&new_metadata)?)?;

                Ok(ProviderPlugin {
                    plugin_type: discovered_type,
                    file_path: blob_path,
                })
            }
            Err(e) => {
                match e {
                    OciDistributionError::RegistryError { envelope, url } => {
                        // FIXME: errors is a Vec<> so need to check the others
                        for e in envelope.errors {
                            if e.code == OciErrorCode::Denied {
                                return Err(format!(
                                    "Access denied for '{:?}': {}",
                                    url, e.message
                                )
                                .into());
                            } else if e.code == OciErrorCode::Unauthorized {
                                return Err(format!(
                                    "Unauthorized access to '{:?}': {}",
                                    url, e.message
                                )
                                .into());
                            } else {
                                return Err(format!(
                                    "Error while accessing '{:?}': {}",
                                    url, e.message
                                )
                                .into());
                            }
                        }
                    }
                    OciDistributionError::UnauthorizedError { url } => {
                        return Err(format!("Unauthorized access to {:?}", url).into());
                    }
                    OciDistributionError::AuthenticationFailure(err) => {
                        return Err(format!("Authentication failure: {:?}", err).into());
                    }
                    _ => todo!("{:?}", e),
                }

                if let Some(meta) = local_metadata {
                    let blob_path =
                        get_blob_path(cache_path, &meta.manifest_digest, &meta.filename);
                    if blob_path.exists() {
                        log::debug!("OFFLINE CACHE HIT: Using stale local version.");
                        return load_from_cache(&meta, &blob_path);
                    }
                }
                Err("No internet connection and no cached version available for this image.".into())
            }
        }
    }
}

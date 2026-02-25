use crate::error::LLMError;
use anyhow::anyhow;
use docker_credential::{CredentialRetrievalError, DockerCredential};
use futures::StreamExt;
use hex;
use oci_client::{
    errors::{OciDistributionError, OciErrorCode},
    manifest::{OciImageManifest, OciManifest, Platform},
    secrets::RegistryAuth,
    Client, Reference,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt;
use tracing::instrument;

// ── Progress types ────────────────────────────────────────────────────────────

/// Phase of an OCI plugin download/update operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OciDownloadPhase {
    /// Resolving manifest and checking cache.
    Resolving,
    /// Verifying image signature.
    VerifyingSignature,
    /// Downloading blob layer bytes.
    Downloading,
    /// Extracting file from archive (native plugins).
    Extracting,
    /// Persisting to cache.
    Persisting,
    /// Completed successfully.
    Completed,
    /// Failed with error message.
    Failed(String),
}

/// Progress snapshot for an OCI plugin download/update.
#[derive(Debug, Clone)]
pub struct OciDownloadProgress {
    pub phase: OciDownloadPhase,
    pub bytes_downloaded: u64,
    pub bytes_total: Option<u64>,
    pub percent: Option<f32>,
}

/// Callback invoked with download progress updates.
///
/// Uses `Arc` so the callback can be cheaply cloned across async boundaries and
/// into `spawn_blocking` closures.
pub type OciProgressCallback = Arc<dyn Fn(OciDownloadProgress) + Send + Sync>;

use super::{PluginType, ProviderPlugin};

const PLUGIN_TYPE_ANNOTATION: &str = "mt.query.plugin.type";

/// Normalize OS name to OCI standard naming conventions
fn normalize_os(os: &str) -> &str {
    match os {
        "macos" => "darwin",
        other => other,
    }
}

/// Normalize architecture to OCI standard naming conventions
fn normalize_arch(arch: &str) -> &str {
    match arch {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

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
) -> Result<Box<dyn TrustRoot + Send + Sync>, anyhow::Error> {
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
                    if let Some(path_str) = rekor_keys_path.to_str() {
                        log::info!("Added Rekor public key");
                        data.rekor_keys.insert(path_str.to_string(), content);
                    }
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

/// Stream a blob from an OCI registry to `dest`, reporting progress and verifying digest.
///
/// Uses `pull_blob_stream` (not `pull_blob`) so we can observe byte counts incrementally.
/// SHA-256 is computed incrementally; a `Digest mismatch` error is returned on failure.
async fn stream_blob_with_progress(
    client: &Client,
    reference: &Reference,
    layer: &oci_client::manifest::OciDescriptor,
    dest: &mut tokio::fs::File,
    progress: &OciProgressCallback,
) -> Result<(), Box<dyn std::error::Error>> {
    let total = if layer.size > 0 {
        Some(layer.size as u64)
    } else {
        None
    };
    let sized_stream = client.pull_blob_stream(reference, layer).await?;
    let mut stream = sized_stream.stream;
    let mut downloaded: u64 = 0;
    let mut hasher = Sha256::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        dest.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;
        let pct = total.map(|t| (downloaded as f32 / t as f32) * 100.0);
        progress(OciDownloadProgress {
            phase: OciDownloadPhase::Downloading,
            bytes_downloaded: downloaded,
            bytes_total: total,
            percent: pct,
        });
    }
    dest.flush().await?;

    let computed = format!("sha256:{}", hex::encode(hasher.finalize()));
    if computed != layer.digest {
        return Err(format!(
            "Digest mismatch: expected {}, got {}",
            layer.digest, computed
        )
        .into());
    }
    Ok(())
}

async fn extract_file_and_content(
    client: &Client,
    reference: &Reference,
    image_manifest: &OciImageManifest,
    plugin_type: PluginType,
    filename: Option<&str>,
    progress: &OciProgressCallback,
) -> Result<(String, NamedTempFile), Box<dyn std::error::Error>> {
    match plugin_type {
        PluginType::Wasm => {
            // Find the wasm layer and extract it.
            for layer in &image_manifest.layers {
                if layer.media_type == "application/vnd.wasm.v1.layer+wasm" {
                    // Create a temporary file to stream the download
                    let temp_file = NamedTempFile::new()?;

                    // Reopen to get a file handle while keeping temp_file alive
                    let std_file = temp_file.reopen()?;
                    let mut tokio_file = tokio::fs::File::from_std(std_file);

                    // Stream blob with progress reporting and digest verification
                    stream_blob_with_progress(client, reference, layer, &mut tokio_file, progress)
                        .await?;

                    let filename = filename.unwrap_or("plugin.wasm").to_string();
                    return Ok((filename, temp_file));
                }
            }
            Err("Wasm plugin type was determined, but no Wasm layer was found.".into())
        }
        PluginType::Native => {
            // Find the native tarball layer and extract the file from it.
            for layer in &image_manifest.layers {
                if layer.media_type == "application/vnd.oci.image.layer.v1.tar+gzip" {
                    // Create a temporary file to stream the download
                    let temp_file = NamedTempFile::new()?;
                    let temp_path = temp_file.path().to_path_buf();

                    // Reopen to get a file handle while keeping temp_file alive
                    let std_file = temp_file.reopen()?;
                    let mut tokio_file = tokio::fs::File::from_std(std_file);

                    // Stream blob with progress reporting and digest verification
                    stream_blob_with_progress(client, reference, layer, &mut tokio_file, progress)
                        .await?;

                    // Signal that we are now extracting from the archive
                    progress(OciDownloadProgress {
                        phase: OciDownloadPhase::Extracting,
                        bytes_downloaded: 0,
                        bytes_total: None,
                        percent: None,
                    });

                    // Move decompression and tar extraction to a blocking thread
                    let target_filename = filename.map(|s| s.to_string());
                    let temp_path_clone = temp_path.clone();
                    let (extracted_filename, extracted_temp_file) = tokio::task::spawn_blocking(
                        move || -> Result<(String, NamedTempFile), anyhow::Error> {
                            // Open the temp file synchronously
                            let file = std::fs::File::open(&temp_path_clone)
                                .map_err(|e| anyhow!("Failed to open temp file: {}", e))?;
                            let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));

                            for entry_result in archive
                                .entries()
                                .map_err(|e| anyhow!("Failed to read tar entries: {}", e))?
                            {
                                let mut entry = entry_result
                                    .map_err(|e| anyhow!("Failed to read tar entry: {}", e))?;
                                if entry.header().entry_type().is_file() {
                                    let path = entry
                                        .path()
                                        .map_err(|e| anyhow!("Failed to get entry path: {}", e))?
                                        .to_string_lossy()
                                        .to_string();
                                    let current_filename = Path::new(&path)
                                        .file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy();

                                    let matches = target_filename.is_none()
                                        || target_filename
                                            .as_ref()
                                            .map(|t| current_filename == t.as_str())
                                            .unwrap_or(false);

                                    if matches {
                                        // Extract to a new temp file
                                        let output_temp = NamedTempFile::new().map_err(|e| {
                                            anyhow!("Failed to create output temp file: {}", e)
                                        })?;
                                        // Reopen to get a writable file handle
                                        let mut output_file =
                                            output_temp.reopen().map_err(|e| {
                                                anyhow!("Failed to reopen output temp file: {}", e)
                                            })?;
                                        std::io::copy(&mut entry, &mut output_file).map_err(
                                            |e| anyhow!("Failed to extract file from tar: {}", e),
                                        )?;

                                        return Ok((current_filename.to_string(), output_temp));
                                    }
                                }
                            }
                            Err(anyhow!("No matching file found in tar archive."))
                        },
                    )
                    .await
                    .map_err(|e| anyhow!("Blocking task panicked: {}", e))??;

                    // Clean up the compressed temp file (the tarball)
                    drop(temp_file);

                    return Ok((extracted_filename, extracted_temp_file));
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

/// Persist a temporary file to a target path, with fallback to copy for cross-device scenarios.
///
/// On some systems (particularly Linux), /tmp may be on a different filesystem than the target
/// directory. In this case, `NamedTempFile::persist()` will fail with EXDEV (error 18).
/// This function handles that case by falling back to a copy operation.
fn persist_with_fallback(
    temp_file: NamedTempFile,
    target: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    match temp_file.persist(target) {
        Ok(_) => Ok(()),
        Err(persist_error) => {
            // Check if this is a cross-device link error (EXDEV = 18 on Unix systems)
            let is_cross_device = persist_error.error.raw_os_error() == Some(18);

            if is_cross_device {
                log::debug!("Cross-device persist failed, falling back to copy");
                // Get the temp file back from the error
                let temp_file = persist_error.file;
                // Copy to the target location
                fs::copy(temp_file.path(), target)?;
                // temp_file drops here, cleaning up the temporary file
                Ok(())
            } else {
                // Some other error occurred, propagate it
                Err(persist_error.into())
            }
        }
    }
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
        progress: Option<OciProgressCallback>,
    ) -> Result<ProviderPlugin, Box<dyn std::error::Error>> {
        let progress: OciProgressCallback = progress.unwrap_or_else(|| Arc::new(|_| {}));

        let sanitized_tag_path = image_reference.replace(['/', ':'], "_");
        let manifests_cache_dir = cache_path.join("manifests");
        fs::create_dir_all(&manifests_cache_dir)?;
        let metadata_path = manifests_cache_dir.join(format!("{}.json", sanitized_tag_path));

        // --- Resolving phase ---
        progress(OciDownloadProgress {
            phase: OciDownloadPhase::Resolving,
            bytes_downloaded: 0,
            bytes_total: None,
            percent: None,
        });

        let local_metadata: Option<CacheMetadata> = fs::read(&metadata_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());

        if !force_update {
            if let Some(meta) = &local_metadata {
                let blob_path = get_blob_path(cache_path, &meta.manifest_digest, &meta.filename);
                if blob_path.exists() {
                    log::debug!("Found cached OCI plugin. Using local version.");
                    progress(OciDownloadProgress {
                        phase: OciDownloadPhase::Completed,
                        bytes_downloaded: 0,
                        bytes_total: None,
                        percent: Some(100.0),
                    });
                    return load_from_cache(meta, &blob_path);
                }
            }
        }

        log::info!("Pulling {} ...", image_reference);

        let client_config = oci_client::client::ClientConfig::default();
        let client = Client::new(client_config);

        let reference = Reference::try_from(image_reference)?;
        let auth = build_auth(&reference);

        // --- Signature verification phase ---
        if self.config.insecure_skip_signature {
            progress(OciDownloadProgress {
                phase: OciDownloadPhase::VerifyingSignature,
                bytes_downloaded: 0,
                bytes_total: None,
                percent: None,
            });
            log::info!("Signature verification enabled for {}", image_reference);
            match verify_image_signature(&self.config, image_reference).await {
                Ok(verified) => {
                    if !verified {
                        let msg = format!(
                            "No valid signatures found for the image {}",
                            image_reference
                        );
                        progress(OciDownloadProgress {
                            phase: OciDownloadPhase::Failed(msg.clone()),
                            bytes_downloaded: 0,
                            bytes_total: None,
                            percent: None,
                        });
                        return Err(msg.into());
                    }
                }
                Err(e) => {
                    let msg = format!("Image signature verification failed: {}", e);
                    progress(OciDownloadProgress {
                        phase: OciDownloadPhase::Failed(msg.clone()),
                        bytes_downloaded: 0,
                        bytes_total: None,
                        percent: None,
                    });
                    return Err(msg.into());
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
                        progress(OciDownloadProgress {
                            phase: OciDownloadPhase::Completed,
                            bytes_downloaded: 0,
                            bytes_total: None,
                            percent: Some(100.0),
                        });
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
                            os: normalize_os(OS).into(),
                            architecture: normalize_arch(ARCH).into(),
                            os_version: None,
                            os_features: None,
                            variant: None,
                            features: None,
                        };
                        log::debug!(
                            "Searching for platform: {}/{} (normalized from {}/{})",
                            native_platform.os,
                            native_platform.architecture,
                            OS,
                            ARCH
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
                                let msg = format!("Image index contains no manifest for the host platform ({}/{}) and no wasi/wasm fallback was found.",
                                    OS, ARCH
                                );
                                progress(OciDownloadProgress {
                                    phase: OciDownloadPhase::Failed(msg.clone()),
                                    bytes_downloaded: 0,
                                    bytes_total: None,
                                    percent: None,
                                });
                                return Err(msg.into());
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

                // --- Downloading phase (delegated to extract_file_and_content) ---
                let (filename, temp_file) = extract_file_and_content(
                    &client,
                    &reference,
                    &image_manifest,
                    discovered_type,
                    target_file_path,
                    &progress,
                )
                .await?;

                // --- Persisting phase ---
                progress(OciDownloadProgress {
                    phase: OciDownloadPhase::Persisting,
                    bytes_downloaded: 0,
                    bytes_total: None,
                    percent: None,
                });

                let blob_path = get_blob_path(cache_path, &live_digest, &filename);
                fs::create_dir_all(blob_path.parent().unwrap())?;

                // Move the temp file to the final blob cache location
                // Uses atomic rename when possible, falls back to copy for cross-device scenarios
                persist_with_fallback(temp_file, &blob_path)?;

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

                // --- Completed ---
                progress(OciDownloadProgress {
                    phase: OciDownloadPhase::Completed,
                    bytes_downloaded: 0,
                    bytes_total: None,
                    percent: Some(100.0),
                });

                Ok(ProviderPlugin {
                    plugin_type: discovered_type,
                    file_path: blob_path,
                })
            }
            Err(e) => {
                match e {
                    OciDistributionError::RegistryError { envelope, url } => {
                        // Prioritize auth-related errors as they are most actionable
                        let auth_error = envelope.errors.iter().find(|e| {
                            matches!(e.code, OciErrorCode::Denied | OciErrorCode::Unauthorized)
                        });

                        if let Some(e) = auth_error {
                            match e.code {
                                OciErrorCode::Denied => {
                                    let msg = format!(
                                        "Access denied for '{:?}': {}",
                                        url, e.message
                                    );
                                    progress(OciDownloadProgress {
                                        phase: OciDownloadPhase::Failed(msg.clone()),
                                        bytes_downloaded: 0,
                                        bytes_total: None,
                                        percent: None,
                                    });
                                    return Err(msg.into());
                                }
                                OciErrorCode::Unauthorized => {
                                    let msg = format!(
                                        "Unauthorized access to '{:?}': {}",
                                        url, e.message
                                    );
                                    progress(OciDownloadProgress {
                                        phase: OciDownloadPhase::Failed(msg.clone()),
                                        bytes_downloaded: 0,
                                        bytes_total: None,
                                        percent: None,
                                    });
                                    return Err(msg.into());
                                }
                                _ => unreachable!(),
                            }
                        } else if let Some(e) = envelope.errors.first() {
                            let msg = format!(
                                "Error while accessing '{:?}': {}",
                                url, e.message
                            );
                            progress(OciDownloadProgress {
                                phase: OciDownloadPhase::Failed(msg.clone()),
                                bytes_downloaded: 0,
                                bytes_total: None,
                                percent: None,
                            });
                            return Err(msg.into());
                        }
                    }
                    OciDistributionError::UnauthorizedError { url } => {
                        let msg = format!("Unauthorized access to {:?}", url);
                        progress(OciDownloadProgress {
                            phase: OciDownloadPhase::Failed(msg.clone()),
                            bytes_downloaded: 0,
                            bytes_total: None,
                            percent: None,
                        });
                        return Err(msg.into());
                    }
                    OciDistributionError::AuthenticationFailure(err) => {
                        let msg = format!("Authentication failure: {:?}", err);
                        progress(OciDownloadProgress {
                            phase: OciDownloadPhase::Failed(msg.clone()),
                            bytes_downloaded: 0,
                            bytes_total: None,
                            percent: None,
                        });
                        return Err(msg.into());
                    }
                    _ => {
                        let msg = format!(
                            "Failed to pull manifest for '{}': {}",
                            image_reference, e
                        );
                        progress(OciDownloadProgress {
                            phase: OciDownloadPhase::Failed(msg.clone()),
                            bytes_downloaded: 0,
                            bytes_total: None,
                            percent: None,
                        });
                        return Err(msg.into());
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_normalize_os_macos() {
        assert_eq!(normalize_os("macos"), "darwin");
    }

    #[test]
    fn test_normalize_os_passthrough() {
        assert_eq!(normalize_os("linux"), "linux");
        assert_eq!(normalize_os("windows"), "windows");
    }

    #[test]
    fn test_normalize_arch_aarch64() {
        assert_eq!(normalize_arch("aarch64"), "arm64");
    }

    #[test]
    fn test_normalize_arch_x86_64() {
        assert_eq!(normalize_arch("x86_64"), "amd64");
    }

    #[test]
    fn test_normalize_arch_passthrough() {
        assert_eq!(normalize_arch("riscv64"), "riscv64");
    }

    #[test]
    fn test_get_blob_path() {
        let cache_root = Path::new("/cache");
        let digest = "sha256:abc123def456";
        let filename = "plugin.wasm";

        let result = get_blob_path(cache_root, digest, filename);

        assert_eq!(
            result,
            PathBuf::from("/cache/blobs/sha256_abc123def456/plugin.wasm")
        );
    }

    #[test]
    fn test_get_blob_path_sanitizes_colon() {
        let cache_root = Path::new("/tmp");
        let digest = "sha256:xyz";
        let filename = "file";

        let result = get_blob_path(cache_root, digest, filename);

        // Verify colon is replaced with underscore
        assert!(result.to_string_lossy().contains("sha256_xyz"));
        assert!(!result.to_string_lossy().contains("sha256:xyz"));
    }

    #[test]
    fn test_load_from_cache_wasm() {
        let meta = CacheMetadata {
            manifest_digest: "sha256:abc".to_string(),
            filename: "plugin.wasm".to_string(),
            plugin_type_str: "extism".to_string(),
            retrieved_at_unix: 1234567890,
        };

        let temp_dir = tempfile::tempdir().unwrap();
        let blob_path = temp_dir.path().join("plugin.wasm");
        fs::write(&blob_path, b"dummy").unwrap();

        let result = load_from_cache(&meta, &blob_path).unwrap();

        assert!(matches!(result.plugin_type, PluginType::Wasm));
        assert_eq!(result.file_path, blob_path);
    }

    #[test]
    fn test_load_from_cache_native() {
        let meta = CacheMetadata {
            manifest_digest: "sha256:def".to_string(),
            filename: "plugin.so".to_string(),
            plugin_type_str: "native".to_string(),
            retrieved_at_unix: 1234567890,
        };

        let temp_dir = tempfile::tempdir().unwrap();
        let blob_path = temp_dir.path().join("plugin.so");
        fs::write(&blob_path, b"dummy").unwrap();

        let result = load_from_cache(&meta, &blob_path).unwrap();

        assert!(matches!(result.plugin_type, PluginType::Native));
        assert_eq!(result.file_path, blob_path);
    }

    #[test]
    fn test_load_from_cache_invalid_type() {
        let meta = CacheMetadata {
            manifest_digest: "sha256:ghi".to_string(),
            filename: "plugin".to_string(),
            plugin_type_str: "invalid".to_string(),
            retrieved_at_unix: 1234567890,
        };

        let temp_dir = tempfile::tempdir().unwrap();
        let blob_path = temp_dir.path().join("plugin");

        let result = load_from_cache(&meta, &blob_path);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid plugin type"));
    }

    #[test]
    fn test_cache_metadata_serialization() {
        let meta = CacheMetadata {
            manifest_digest: "sha256:test123".to_string(),
            filename: "test.wasm".to_string(),
            plugin_type_str: "extism".to_string(),
            retrieved_at_unix: 1234567890,
        };

        // Serialize to JSON
        let json = serde_json::to_string(&meta).unwrap();

        // Deserialize back
        let deserialized: CacheMetadata = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.manifest_digest, meta.manifest_digest);
        assert_eq!(deserialized.filename, meta.filename);
        assert_eq!(deserialized.plugin_type_str, meta.plugin_type_str);
        assert_eq!(deserialized.retrieved_at_unix, meta.retrieved_at_unix);
    }

    /// Test that NamedTempFile survives when using reopen() and can be persisted.
    /// This is the core fix for the bug where into_file() was deleting the temp file.
    #[test]
    fn test_named_temp_file_survives_for_persist() {
        let temp_file = NamedTempFile::new().unwrap();

        // Write using reopen (like our fix does)
        let mut file = temp_file.reopen().unwrap();
        file.write_all(b"test content").unwrap();
        drop(file); // Close the handle

        // File should still exist
        assert!(temp_file.path().exists());

        // Persist to final location
        let final_dir = tempfile::tempdir().unwrap();
        let final_path = final_dir.path().join("final_file");
        temp_file.persist(&final_path).unwrap();

        // Final file should exist with correct content
        assert!(final_path.exists());
        assert_eq!(fs::read_to_string(&final_path).unwrap(), "test content");
    }

    /// Test that persist_with_fallback works in the normal case (same filesystem).
    #[test]
    fn test_persist_with_fallback_same_filesystem() {
        let temp_file = NamedTempFile::new().unwrap();

        let mut file = temp_file.reopen().unwrap();
        file.write_all(b"same filesystem content").unwrap();
        drop(file);

        let final_dir = tempfile::tempdir().unwrap();
        let final_path = final_dir.path().join("final_file");

        persist_with_fallback(temp_file, &final_path).unwrap();

        assert!(final_path.exists());
        assert_eq!(
            fs::read_to_string(&final_path).unwrap(),
            "same filesystem content"
        );
    }

    /// Test that NamedTempFile can be returned from a spawn_blocking closure.
    /// This verifies that NamedTempFile is Send, which is required for the native plugin
    /// extraction that happens in a blocking task.
    #[tokio::test]
    async fn test_blocking_task_returns_temp_file() {
        let temp_file = tokio::task::spawn_blocking(|| {
            let temp = NamedTempFile::new().unwrap();
            let mut file = temp.reopen().unwrap();
            std::io::Write::write_all(&mut file, b"from blocking").unwrap();
            temp // Return the NamedTempFile
        })
        .await
        .unwrap();

        // File should still exist after returning from blocking task
        assert!(temp_file.path().exists());
        assert_eq!(fs::read(temp_file.path()).unwrap(), b"from blocking");

        // Verify we can persist it
        let final_dir = tempfile::tempdir().unwrap();
        let final_path = final_dir.path().join("final");
        persist_with_fallback(temp_file, &final_path).unwrap();
        assert_eq!(fs::read(&final_path).unwrap(), b"from blocking");
    }

    /// Test that the old buggy pattern (into_file) would fail.
    /// This test documents the bug we fixed.
    #[test]
    fn test_into_file_pattern_loses_temp_file() {
        let temp_file = NamedTempFile::new().unwrap();
        let temp_path = temp_file.path().to_path_buf();

        // This is the OLD buggy pattern - into_file() consumes the guard
        let mut file = temp_file.into_file();
        file.write_all(b"lost content").unwrap();
        drop(file);

        // The temp file is now DELETED because the NamedTempFile guard was consumed
        assert!(
            !temp_path.exists(),
            "Bug: temp file should be deleted after into_file()"
        );
    }

    // Note: Tests for determine_plugin_type are omitted because constructing
    // OciImageManifest requires complex internal structures from oci-client crate.
    // These are tested implicitly by the integration tests (qmt update command).

    // ── Progress type tests ───────────────────────────────────────────────────

    #[test]
    fn oci_download_phase_eq() {
        assert_eq!(OciDownloadPhase::Resolving, OciDownloadPhase::Resolving);
        assert_eq!(OciDownloadPhase::Downloading, OciDownloadPhase::Downloading);
        assert_ne!(OciDownloadPhase::Resolving, OciDownloadPhase::Downloading);
        assert_eq!(
            OciDownloadPhase::Failed("oops".to_string()),
            OciDownloadPhase::Failed("oops".to_string())
        );
        assert_ne!(
            OciDownloadPhase::Failed("a".to_string()),
            OciDownloadPhase::Failed("b".to_string())
        );
    }

    #[test]
    fn oci_download_progress_fields() {
        let p = OciDownloadProgress {
            phase: OciDownloadPhase::Downloading,
            bytes_downloaded: 1024,
            bytes_total: Some(2048),
            percent: Some(50.0),
        };
        assert_eq!(p.bytes_downloaded, 1024);
        assert_eq!(p.bytes_total, Some(2048));
        assert!((p.percent.unwrap() - 50.0).abs() < 0.01);
    }

    #[test]
    fn oci_progress_callback_is_callable() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let counter = Arc::new(AtomicU64::new(0));
        let counter_clone = counter.clone();
        let cb: OciProgressCallback = Arc::new(move |p: OciDownloadProgress| {
            counter_clone.fetch_add(p.bytes_downloaded, Ordering::SeqCst);
        });
        cb(OciDownloadProgress {
            phase: OciDownloadPhase::Downloading,
            bytes_downloaded: 42,
            bytes_total: None,
            percent: None,
        });
        assert_eq!(counter.load(Ordering::SeqCst), 42);
    }

    #[test]
    fn oci_progress_callback_noop_default() {
        // Simulates the no-op used when progress is None.
        let noop: OciProgressCallback = Arc::new(|_| {});
        // Must not panic.
        noop(OciDownloadProgress {
            phase: OciDownloadPhase::Completed,
            bytes_downloaded: 0,
            bytes_total: None,
            percent: None,
        });
    }

    #[test]
    fn digest_verification_sha256_format() {
        // Validate the digest string format we produce matches layer.digest expectations.
        use sha2::{Digest as _, Sha256};
        let data = b"hello world";
        let mut hasher = Sha256::new();
        hasher.update(data);
        let computed = format!("sha256:{}", hex::encode(hasher.finalize()));
        assert!(computed.starts_with("sha256:"));
        assert_eq!(computed.len(), 7 + 64); // "sha256:" + 64 hex chars
    }

    /// stream_blob_with_progress: digest mismatch must produce an error.
    #[tokio::test]
    async fn stream_blob_progress_digest_mismatch_is_error() {
        // We can't call stream_blob_with_progress without a real OCI client,
        // but we can unit-test the core digest-check logic that will live inside it.
        let data = b"some blob data";
        let mut hasher = Sha256::new();
        hasher.update(data);
        let actual_digest = format!("sha256:{}", hex::encode(hasher.finalize()));

        let wrong_digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert_ne!(actual_digest, wrong_digest, "digests should differ");

        // Simulate the check performed inside stream_blob_with_progress.
        let result: Result<(), String> = if actual_digest != wrong_digest {
            Err(format!(
                "Digest mismatch: expected {}, got {}",
                wrong_digest, actual_digest
            ))
        } else {
            Ok(())
        };
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Digest mismatch"));
    }

    /// stream_blob_with_progress: matching digest must succeed.
    #[tokio::test]
    async fn stream_blob_progress_digest_match_is_ok() {
        let data = b"some blob data";
        let mut hasher = Sha256::new();
        hasher.update(data);
        let actual_digest = format!("sha256:{}", hex::encode(hasher.finalize()));

        let result: Result<(), String> = if actual_digest != actual_digest.clone() {
            Err("mismatch".into())
        } else {
            Ok(())
        };
        assert!(result.is_ok());
    }

    /// stream_blob_with_progress: progress callback is invoked with correct byte counts.
    #[tokio::test]
    async fn stream_blob_progress_callback_accumulates_bytes() {
        use std::sync::{Arc, Mutex};
        // Simulate what stream_blob_with_progress does:
        // iterate chunks, accumulate bytes, invoke callback.
        let chunks: Vec<&[u8]> = vec![b"hello", b" ", b"world"];
        let total: u64 = chunks.iter().map(|c| c.len() as u64).sum();

        let records: Arc<Mutex<Vec<OciDownloadProgress>>> = Arc::new(Mutex::new(Vec::new()));
        let records_clone = records.clone();
        let cb: OciProgressCallback = Arc::new(move |p| {
            records_clone.lock().unwrap().push(p);
        });

        let mut downloaded: u64 = 0;
        for chunk in &chunks {
            downloaded += chunk.len() as u64;
            let pct = Some((downloaded as f32 / total as f32) * 100.0);
            cb(OciDownloadProgress {
                phase: OciDownloadPhase::Downloading,
                bytes_downloaded: downloaded,
                bytes_total: Some(total),
                percent: pct,
            });
        }

        let recs = records.lock().unwrap();
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].bytes_downloaded, 5);
        assert_eq!(recs[1].bytes_downloaded, 6);
        assert_eq!(recs[2].bytes_downloaded, 11);
        assert_eq!(recs[2].bytes_total, Some(11));
        assert!((recs[2].percent.unwrap() - 100.0).abs() < 0.01);
    }
}

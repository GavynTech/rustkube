//! Registry pulls via `oci-client`: resolve the manifest for linux/amd64,
//! fetch config + layer blobs, and verify every blob's sha256 against the
//! digest the manifest promised before anything is stored or extracted.

use oci_client::client::{ClientConfig, linux_amd64_resolver};
use oci_client::manifest::{OciDescriptor, OciImageManifest};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference as OciReference};
use sha2::{Digest as _, Sha256};

use crate::error::{Result, RuntimeError};
use crate::image::{ImageRef, Reference};
use crate::layers;
use crate::store::{ImageRecord, Store};

#[derive(Debug)]
pub struct PulledLayer {
    pub digest: String,
    pub size_bytes: usize,
    pub reused: bool,
}

#[derive(Debug)]
pub struct PullReport {
    pub reference: String,
    pub manifest_digest: String,
    pub config_digest: String,
    pub layers: Vec<PulledLayer>,
}

/// The registry stack is x86_64-only for now — the resolver picks the
/// linux/amd64 entry out of multi-arch indexes, matching the Ubuntu VM.
fn client() -> Client {
    Client::new(ClientConfig {
        platform_resolver: Some(Box::new(linux_amd64_resolver)),
        ..ClientConfig::default()
    })
}

fn oci_reference(image: &ImageRef) -> OciReference {
    let registry = image.registry_host().to_owned();
    let repository = image.repository.clone();
    match &image.reference {
        Reference::Tag(tag) => OciReference::with_tag(registry, repository, tag.clone()),
        Reference::Digest(digest) => {
            OciReference::with_digest(registry, repository, digest.clone())
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn verify_digest(what: &str, expected: &str, bytes: &[u8]) -> Result<()> {
    let actual = sha256_hex(bytes);
    if actual != expected {
        return Err(RuntimeError::DigestMismatch {
            what: what.to_owned(),
            expected: expected.to_owned(),
            actual,
        });
    }
    Ok(())
}

async fn fetch_blob(
    client: &Client,
    reference: &OciReference,
    descriptor: &OciDescriptor,
    what: &str,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    client
        .pull_blob(reference, descriptor, &mut bytes)
        .await
        .map_err(|source| RuntimeError::Registry {
            reference: reference.to_string(),
            source,
        })?;
    verify_digest(what, &descriptor.digest, &bytes)?;
    Ok(bytes)
}

/// Pull an image into the store: manifest, config and all layers, with every
/// blob digest-verified. Already-present layers are neither re-downloaded nor
/// re-extracted. Returns what was done for the CLI to report.
pub async fn pull(store: &Store, image: &ImageRef) -> Result<PullReport> {
    store.ensure_layout()?;
    let client = client();
    let reference = oci_reference(image);

    let (manifest, manifest_digest): (OciImageManifest, String) = client
        .pull_image_manifest(&reference, &RegistryAuth::Anonymous)
        .await
        .map_err(|source| RuntimeError::Registry {
            reference: reference.to_string(),
            source,
        })?;
    tracing::info!(image = %image, digest = %manifest_digest, "resolved manifest");

    // If the user pinned a digest, the registry must hand back that manifest.
    if let Reference::Digest(pinned) = &image.reference
        && pinned != &manifest_digest
    {
        return Err(RuntimeError::DigestMismatch {
            what: "pinned manifest".to_owned(),
            expected: pinned.clone(),
            actual: manifest_digest,
        });
    }

    let manifest_json = serde_json::to_vec_pretty(&manifest).map_err(|source| {
        RuntimeError::json("image manifest", store.root().join("images"), source)
    })?;
    let manifest_path = store.manifest_path(&manifest_digest)?;
    std::fs::write(&manifest_path, &manifest_json)
        .map_err(|source| RuntimeError::io("writing manifest", &manifest_path, source))?;

    let config_bytes = fetch_blob(&client, &reference, &manifest.config, "image config").await?;
    store.write_blob(&manifest.config.digest, &config_bytes)?;

    let mut layers_report = Vec::with_capacity(manifest.layers.len());
    for descriptor in &manifest.layers {
        let reused = store.has_blob(&descriptor.digest)? && store.has_layer(&descriptor.digest)?;
        if reused {
            tracing::debug!(digest = %descriptor.digest, "layer already present");
        } else {
            let bytes = fetch_blob(&client, &reference, descriptor, "image layer").await?;
            store.write_blob(&descriptor.digest, &bytes)?;
            layers::extract_layer(store, &descriptor.digest, &descriptor.media_type, &bytes)?;
        }
        layers_report.push(PulledLayer {
            digest: descriptor.digest.clone(),
            size_bytes: usize::try_from(descriptor.size).unwrap_or(usize::MAX),
            reused,
        });
    }

    let mut index = store.read_index()?;
    index.insert(
        image.to_string(),
        ImageRecord {
            manifest_digest: manifest_digest.clone(),
            config_digest: manifest.config.digest.clone(),
            layers: manifest.layers.iter().map(|l| l.digest.clone()).collect(),
            pulled_at: chrono::Utc::now().to_rfc3339(),
        },
    );
    store.write_index(&index)?;

    Ok(PullReport {
        reference: image.to_string(),
        manifest_digest,
        config_digest: manifest.config.digest.clone(),
        layers: layers_report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_verification_catches_tampering() {
        let bytes = b"layer data";
        let good = sha256_hex(bytes);
        assert!(verify_digest("layer", &good, bytes).is_ok());

        let err = verify_digest(
            "layer",
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            bytes,
        )
        .unwrap_err();
        assert!(matches!(err, RuntimeError::DigestMismatch { .. }));
    }

    #[test]
    fn oci_reference_uses_the_registry_api_host() {
        let image: ImageRef = "nginx:1.27".parse().unwrap();
        let reference = oci_reference(&image);
        assert_eq!(reference.registry(), "registry-1.docker.io");
        assert_eq!(reference.repository(), "library/nginx");
        assert_eq!(reference.tag(), Some("1.27"));
    }
}

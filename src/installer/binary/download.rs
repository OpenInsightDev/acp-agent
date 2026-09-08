use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use sha2::{Digest, Sha256};

use super::ArchiveLimits;
use super::paths::validate_archive_component;
use crate::registry::BinaryTarget;
pub(crate) async fn download_archive(target: &BinaryTarget, temp_dir: &Path) -> Result<PathBuf> {
    download_archive_with_limits(target, temp_dir, ArchiveLimits::default()).await
}

pub(crate) async fn download_archive_with_limits(
    target: &BinaryTarget,
    temp_dir: &Path,
    limits: ArchiveLimits,
) -> Result<PathBuf> {
    let expected_digest = target
        .sha256
        .as_deref()
        .ok_or_else(|| anyhow!("binary target is missing required sha256 checksum"))?;
    let expected_digest = parse_sha256(expected_digest)?;
    let url = reqwest::Url::parse(&target.archive)
        .with_context(|| format!("invalid archive URL: {}", target.archive))?;
    let archive_name = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|segment| !segment.is_empty())
        .unwrap_or("download.bin");
    validate_archive_component(archive_name)
        .with_context(|| format!("unsafe archive filename in URL: {archive_name}"))?;
    let destination = temp_dir.join(archive_name);

    let client = reqwest::Client::builder()
        .connect_timeout(limits.connect_timeout)
        .read_timeout(limits.read_timeout)
        .timeout(limits.total_timeout)
        .build()
        .context("failed to build archive HTTP client")?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to download archive from {}", target.archive))?;
    let response = response
        .error_for_status()
        .with_context(|| format!("failed to download archive from {}", target.archive))?;
    if let Some(length) = response.content_length()
        && length > limits.max_download_bytes
    {
        bail!(
            "archive exceeds download limit of {} bytes",
            limits.max_download_bytes
        );
    }
    let result: Result<()> = async {
        let mut stream = response.bytes_stream();
        // Keep file ownership in this future. Tokio filesystem writes use
        // detached blocking operations that can outlive cancellation and race
        // TempDir cleanup; a bounded response chunk is written synchronously
        // so the handle is always closed before staging is removed.
        let mut file = std::fs::File::create(&destination)
            .with_context(|| format!("failed to create {}", destination.display()))?;
        let mut digest = Sha256::new();
        let mut total = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| {
                format!("failed to read archive response from {}", target.archive)
            })?;
            total = total.saturating_add(chunk.len() as u64);
            if total > limits.max_download_bytes {
                bail!(
                    "archive exceeds download limit of {} bytes",
                    limits.max_download_bytes
                );
            }
            digest.update(&chunk);
            file.write_all(&chunk).with_context(|| {
                format!(
                    "failed to write downloaded archive to {}",
                    destination.display()
                )
            })?;
        }
        file.flush()?;
        let actual_digest = digest.finalize();
        if actual_digest.as_slice() != expected_digest {
            bail!(
                "sha256 checksum mismatch: expected {}, got {}",
                hex_encode(&expected_digest),
                hex_encode(actual_digest.as_slice())
            );
        }
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = std::fs::remove_file(&destination);
    }
    result.map(|()| destination)
}

/// Verifies downloaded bytes against the registry-declared SHA-256 digest.
///
/// A missing or malformed digest is rejected before any downloaded content is
/// accepted.
#[cfg(test)]
pub(crate) fn verify_sha256(bytes: &[u8], expected: Option<&str>) -> Result<()> {
    let expected =
        expected.ok_or_else(|| anyhow!("binary target is missing required sha256 checksum"))?;
    let expected = parse_sha256(expected)?;
    let actual = Sha256::digest(bytes);
    if actual.as_slice() != expected {
        bail!(
            "sha256 checksum mismatch: expected {}, got {}",
            hex_encode(&expected),
            hex_encode(actual.as_slice())
        );
    }
    Ok(())
}

/// Parses a registry-declared SHA-256 hex string into raw bytes.
pub(crate) fn parse_sha256(value: &str) -> Result<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid sha256 checksum \"{value}\": expected 64 hexadecimal characters");
    }
    let mut digest = [0u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .expect("hex digits were validated above");
    }
    Ok(digest)
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

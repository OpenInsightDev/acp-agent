use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::installer::cache::BinaryCacheLock;

/// Name of the human-readable install log written into the cache root.
///
/// The image ships without a shell, so this file is how install state and
/// failures can be inspected from a container:
/// `docker cp <container>:/cache/acp-agent/agent-install.log .`
const INSTALL_LOG_FILE_NAME: &str = "agent-install.log";
/// Upper bound for the install log; it is append-only and lives in a cache
/// volume that may persist for a long time.
const INSTALL_LOG_MAX_BYTES: u64 = 1024 * 1024;
/// When the cap is hit, the log is rewritten to keep only this tail.
const INSTALL_LOG_TAIL_BYTES: u64 = 256 * 1024;

/// Hard resource limits applied to binary downloads and archive extraction.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ArchiveLimits {
    /// Maximum compressed response bytes accepted from the network.
    max_download_bytes: u64,
    /// Maximum total uncompressed entry bytes written from an archive.
    max_expanded_bytes: u64,
    /// Maximum archive entries, including directories and symlinks.
    max_entries: u64,
    /// Maximum non-directory entries created from an archive.
    max_files: u64,
    /// Maximum time spent establishing the HTTP connection.
    connect_timeout: Duration,
    /// Maximum idle interval while reading the HTTP response.
    read_timeout: Duration,
    /// Maximum end-to-end HTTP request duration.
    total_timeout: Duration,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            max_download_bytes: 256 * 1024 * 1024,
            max_expanded_bytes: 512 * 1024 * 1024,
            max_entries: 10_000,
            max_files: 5_000,
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(30),
            total_timeout: Duration::from_secs(10 * 60),
        }
    }
}

/// A validated binary distribution stored in the local cache.
#[derive(Debug, Clone)]
pub struct CachedBinary {
    /// Resolved executable path within the extracted payload.
    pub executable_path: PathBuf,
    /// Directory containing the extracted payload.
    pub extracted_dir: PathBuf,
    /// Stable cache directory that owns the extracted payload.
    pub cache_dir: PathBuf,
    /// Shared payload-use lease held by runners and served routes.
    #[doc(hidden)]
    pub(crate) cache_use_lease: Option<Arc<BinaryCacheLock>>,
}

impl PartialEq for CachedBinary {
    fn eq(&self, other: &Self) -> bool {
        self.executable_path == other.executable_path
            && self.extracted_dir == other.extracted_dir
            && self.cache_dir == other.cache_dir
    }
}

impl Eq for CachedBinary {}

mod archive;
mod download;
mod install;
mod log;
mod paths;
mod publication;
mod recovery;
mod staging;
mod validation;

pub use install::cache_binary_target;
pub(crate) use install::refresh_binary_target_in;
pub use recovery::clean_stale_staging_entries;

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use sha2::{Digest, Sha256};
    use tokio::fs;

    use super::archive::{extract_archive_blocking, extract_zip, extract_zip_with_limits};
    use super::download::{
        download_archive, download_archive_with_limits, hex_encode, verify_sha256,
    };
    use super::install::cache_binary_target_in_mode;
    use super::log::{append_install_log_inner, install_log_lock_path, utc_timestamp};
    use super::paths::resolve_cmd_path;
    use super::publication::{promote_prepared_cache, promote_staged_cache};
    use super::recovery::clean_stale_staging_entries_in;
    use super::staging::PreparedStaging;
    use super::validation::{hash_payload_sha256, make_executable, validate_cached_binary};
    use super::{ArchiveLimits, CachedBinary, INSTALL_LOG_MAX_BYTES};
    use crate::installer::cache::{
        BinaryCacheMetadata, EXTRACTED_DIR_NAME, METADATA_FILE_NAME, acquire_binary_cache_lock,
        acquire_binary_cache_use_read_lock, acquire_binary_cache_use_write_lock,
        binary_cache_paths, binary_cache_paths_with_digest,
    };
    use crate::registry::{
        AgentDistribution, BinaryDistribution, BinaryTarget, Platform, RegistryAgent,
    };
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn rejects_download_without_declared_sha256() {
        let error = verify_sha256(b"payload", None).unwrap_err();
        assert!(error.to_string().contains("missing required sha256"));
    }

    #[test]
    fn accepts_download_matching_declared_sha256() {
        let payload: &[u8] = b"payload";
        let digest = hex_encode(Sha256::digest(payload).as_slice());
        verify_sha256(payload, Some(&digest)).expect("matching digest should pass");
    }

    #[test]
    fn rejects_download_mismatching_declared_sha256() {
        let error = verify_sha256(b"payload", Some(&"0".repeat(64))).unwrap_err();
        assert!(error.to_string().contains("sha256 checksum mismatch"));
    }

    #[test]
    fn rejects_malformed_declared_sha256() {
        let error = verify_sha256(b"payload", Some("not-a-sha256")).unwrap_err();
        assert!(error.to_string().contains("invalid sha256 checksum"));
    }

    #[tokio::test]
    async fn rejects_missing_digest_before_url_parsing() {
        let temp_dir = tempdir().unwrap();
        let target = BinaryTarget {
            archive: "not a valid URL".to_string(),
            cmd: "tool".to_string(),
            sha256: None,
            args: None,
            env: None,
        };
        let error = download_archive(&target, temp_dir.path())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("missing required sha256"));
        assert!(temp_dir.path().read_dir().unwrap().next().is_none());
    }

    #[test]
    fn resolves_relative_cmd_paths() {
        let base = Path::new("/tmp/acp-agent");
        let resolved = resolve_cmd_path(base, "./dist-package/cursor-agent").unwrap();
        assert_eq!(resolved, base.join("dist-package").join("cursor-agent"));
    }

    #[test]
    fn rejects_absolute_cmd_paths() {
        let base = Path::new("/tmp/acp-agent");
        let error = resolve_cmd_path(base, "/bin/sh").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("binary command path must be relative")
        );
    }

    #[test]
    fn rejects_parent_dir_cmd_paths() {
        let base = Path::new("/tmp/acp-agent");
        let error = resolve_cmd_path(base, "../bin/sh").unwrap_err();
        assert!(error.to_string().contains("must stay within"));
    }

    #[tokio::test]
    async fn payload_hash_has_unambiguous_tree_boundaries() {
        let temp_dir = tempdir().unwrap();
        let two_files = temp_dir.path().join("two-files");
        let encoded_as_content = temp_dir.path().join("encoded-as-content");
        fs::create_dir_all(&two_files).await.unwrap();
        fs::create_dir_all(&encoded_as_content).await.unwrap();
        fs::write(two_files.join("a"), b"x").await.unwrap();
        fs::write(two_files.join("b"), b"y").await.unwrap();
        // This byte sequence collided with the old delimiter-only encoding:
        // it looks exactly like the end of `a` followed by a complete `b`.
        fs::write(
            encoded_as_content.join("a"),
            [b'x', 0xff, b'b', 0, b'f', 0, b'y'],
        )
        .await
        .unwrap();

        assert_ne!(
            hash_payload_sha256(&two_files).await.unwrap(),
            hash_payload_sha256(&encoded_as_content).await.unwrap()
        );
    }

    #[tokio::test]
    async fn validates_matching_cached_binary() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        let metadata = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/demo.tar.gz",
            "./bin/demo",
            Some("a".repeat(64).as_str()),
        );

        fs::create_dir_all(&paths.extracted_dir).await.unwrap();
        let executable_path = paths.extracted_dir.join("bin").join("demo");
        fs::create_dir_all(executable_path.parent().unwrap())
            .await
            .unwrap();
        fs::write(&executable_path, b"#!/bin/sh\n").await.unwrap();

        let mut metadata = metadata;
        metadata.executable_sha256 = Some(hex_encode(Sha256::digest(b"#!/bin/sh\n").as_slice()));
        metadata.payload_sha256 = Some(hash_payload_sha256(&paths.extracted_dir).await.unwrap());
        fs::write(&paths.metadata_path, serde_json::to_vec(&metadata).unwrap())
            .await
            .unwrap();

        let prepared = validate_cached_binary(&paths, &metadata).await.unwrap();
        assert_eq!(
            prepared.unwrap(),
            CachedBinary {
                executable_path,
                extracted_dir: paths.extracted_dir,
                cache_dir: paths.cache_dir,
                cache_use_lease: None,
            }
        );
    }

    #[tokio::test]
    async fn rejects_cached_binary_when_metadata_mismatches() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        let expected = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/demo.tar.gz",
            "./bin/demo",
            Some("a".repeat(64).as_str()),
        );
        let cached = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/demo.tar.gz",
            "./bin/demo",
            Some("b".repeat(64).as_str()),
        );

        fs::create_dir_all(&paths.extracted_dir).await.unwrap();
        fs::write(&paths.metadata_path, serde_json::to_vec(&cached).unwrap())
            .await
            .unwrap();

        assert!(
            validate_cached_binary(&paths, &expected)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn rejects_modified_digest_bound_executable() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths_with_digest(
            &cache_root,
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            &"a".repeat(64),
        );
        let mut metadata = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/demo.tar.gz",
            "./bin/demo",
            Some(&"a".repeat(64)),
        );
        fs::create_dir_all(paths.extracted_dir.join("bin"))
            .await
            .unwrap();
        let executable_path = paths.extracted_dir.join("bin/demo");
        fs::write(&executable_path, b"original").await.unwrap();
        metadata.executable_sha256 = Some(hex_encode(Sha256::digest(b"original").as_slice()));
        fs::write(&paths.metadata_path, serde_json::to_vec(&metadata).unwrap())
            .await
            .unwrap();

        fs::write(&executable_path, b"modified").await.unwrap();
        assert!(
            validate_cached_binary(&paths, &metadata)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn rejects_modified_digest_bound_payload_file() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths_with_digest(
            &cache_root,
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            &"a".repeat(64),
        );
        let mut metadata = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/demo.tar.gz",
            "./bin/demo",
            Some(&"a".repeat(64)),
        );
        fs::create_dir_all(paths.extracted_dir.join("bin"))
            .await
            .unwrap();
        fs::create_dir_all(paths.extracted_dir.join("lib"))
            .await
            .unwrap();
        let executable_path = paths.extracted_dir.join("bin/demo");
        fs::write(&executable_path, b"original").await.unwrap();
        fs::write(paths.extracted_dir.join("lib/helper"), b"helper")
            .await
            .unwrap();
        metadata.executable_sha256 = Some(hex_encode(Sha256::digest(b"original").as_slice()));
        metadata.payload_sha256 = Some(hash_payload_sha256(&paths.extracted_dir).await.unwrap());
        fs::write(&paths.metadata_path, serde_json::to_vec(&metadata).unwrap())
            .await
            .unwrap();

        fs::write(paths.extracted_dir.join("lib/helper"), b"modified")
            .await
            .unwrap();
        assert!(
            validate_cached_binary(&paths, &metadata)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn corrupted_metadata_is_treated_as_cache_miss_without_unleased_removal() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        let expected = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/demo.tar.gz",
            "./bin/demo",
            Some("a".repeat(64).as_str()),
        );

        fs::create_dir_all(&paths.cache_dir).await.unwrap();
        fs::write(&paths.metadata_path, b"{not-json").await.unwrap();

        assert!(
            validate_cached_binary(&paths, &expected)
                .await
                .unwrap()
                .is_none()
        );
        // Validation does not delete an entry on its own: callers must first
        // take the payload-use write lease so an active reader cannot race
        // the cleanup. Publication replaces this entry under that lease.
        assert!(fs::try_exists(&paths.cache_dir).await.unwrap());
    }

    #[tokio::test]
    async fn failed_promotion_restores_the_previous_cache() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        let previous = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/previous.tar.gz",
            "./bin/demo",
            None,
        );
        let replacement = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/replacement.tar.gz",
            "./bin/demo",
            None,
        );
        fs::create_dir_all(&paths.extracted_dir).await.unwrap();
        fs::write(&paths.metadata_path, serde_json::to_vec(&previous).unwrap())
            .await
            .unwrap();
        let executable = paths.extracted_dir.join("bin").join("demo");
        fs::create_dir_all(executable.parent().unwrap())
            .await
            .unwrap();
        fs::write(&executable, b"previous").await.unwrap();

        let missing_staging = paths.parent_dir.join(".missing-staging");
        assert!(
            promote_staged_cache(&missing_staging, &paths, &replacement, true, None)
                .await
                .is_err()
        );

        assert_eq!(fs::read(&executable).await.unwrap(), b"previous");
        let restored: BinaryCacheMetadata =
            serde_json::from_slice(&fs::read(&paths.metadata_path).await.unwrap()).unwrap();
        assert_eq!(restored, previous);
    }

    #[tokio::test]
    async fn forced_promotion_replaces_cache_with_unchanged_metadata() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        let metadata = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/demo.tar.gz",
            "./demo",
            None,
        );
        fs::create_dir_all(&paths.extracted_dir).await.unwrap();
        fs::write(&paths.metadata_path, serde_json::to_vec(&metadata).unwrap())
            .await
            .unwrap();
        fs::write(paths.extracted_dir.join("demo"), b"old")
            .await
            .unwrap();

        let staging_dir = paths.parent_dir.join(".staging");
        let staging_extracted = staging_dir.join(EXTRACTED_DIR_NAME);
        fs::create_dir_all(&staging_extracted).await.unwrap();
        fs::write(
            staging_dir.join(METADATA_FILE_NAME),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .await
        .unwrap();
        fs::write(staging_extracted.join("demo"), b"new")
            .await
            .unwrap();

        promote_staged_cache(&staging_dir, &paths, &metadata, true, None)
            .await
            .unwrap();

        assert_eq!(
            fs::read(paths.extracted_dir.join("demo")).await.unwrap(),
            b"new"
        );
    }

    #[test]
    fn install_log_appends_lines_in_order() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().join("agent-install.log");

        append_install_log_inner(&path, "first\n").unwrap();
        append_install_log_inner(&path, "second\n").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\nsecond\n");
    }

    #[test]
    fn install_log_lock_is_a_sibling_for_relative_paths() {
        assert_eq!(
            install_log_lock_path(Path::new("cache/agent-install.log")),
            PathBuf::from("cache/agent-install.log.lock")
        );
    }

    #[test]
    fn install_log_appends_concurrently_without_losing_lines() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().join("agent-install.log");
        let thread_count = 16;
        let lines_per_thread = 50;

        let mut threads = Vec::new();
        for t in 0..thread_count {
            let path = path.clone();
            threads.push(std::thread::spawn(move || {
                for i in 0..lines_per_thread {
                    append_install_log_inner(&path, &format!("thread-{t}-line-{i}\n")).unwrap();
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }

        let contents = std::fs::read_to_string(&path).unwrap();
        let mut lines = contents.lines().map(String::from).collect::<Vec<_>>();
        lines.sort_unstable();
        assert_eq!(lines.len(), thread_count * lines_per_thread);

        let mut expected = Vec::with_capacity(thread_count * lines_per_thread);
        for t in 0..thread_count {
            for i in 0..lines_per_thread {
                expected.push(format!("thread-{t}-line-{i}"));
            }
        }
        expected.sort_unstable();
        assert_eq!(lines, expected);
    }

    #[test]
    fn install_log_is_capped_and_keeps_a_complete_tail() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().join("agent-install.log");
        let oversized = "a".repeat(INSTALL_LOG_MAX_BYTES as usize + 64 * 1024);

        append_install_log_inner(&path, &oversized).unwrap();
        append_install_log_inner(&path, &oversized).unwrap();
        append_install_log_inner(&path, "final-marker\n").unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.len() as u64 <= INSTALL_LOG_MAX_BYTES,
            "log exceeded its cap: {}",
            contents.len()
        );
        assert!(contents.starts_with("[install log truncated;"));
        assert!(contents.ends_with("final-marker\n"));
        assert!(
            contents
                .lines()
                .all(|line| line.len() < INSTALL_LOG_MAX_BYTES as usize)
        );
    }

    #[test]
    fn timestamps_are_utc_iso_8601() {
        let timestamp = utc_timestamp();
        assert_eq!(timestamp.len(), 20);
        assert!(timestamp.ends_with('Z'));
        assert_eq!(&timestamp[4..5], "-");
        assert_eq!(&timestamp[7..8], "-");
        assert_eq!(&timestamp[10..11], "T");
        assert_eq!(&timestamp[13..14], ":");
        assert_eq!(&timestamp[16..17], ":");
    }

    #[tokio::test]
    async fn make_executable_preserves_mode_and_adds_only_owner_execute() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempdir().unwrap();
        for (source_mode, expected_mode) in [
            (0o600, 0o700),
            (0o700, 0o700),
            (0o750, 0o750),
            (0o755, 0o755),
        ] {
            let path = temp_dir.path().join(format!("tool-{source_mode:o}"));
            std::fs::write(&path, b"#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(source_mode)).unwrap();

            make_executable(&path).await.unwrap();

            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, expected_mode,
                "mode {source_mode:o} should become {expected_mode:o}"
            );
        }
    }

    // --- ZIP extraction ---

    /// Writes a ZIP archive whose entries are built through the provided
    /// closure, using the crate's own writer.
    fn build_zip_archive(archive_path: &Path, write: impl FnOnce(&mut zip::ZipWriter<File>)) {
        let file = File::create(archive_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        write(&mut writer);
        writer.finish().unwrap();
    }

    /// Hand-builds a single-entry stored (uncompressed) ZIP so tests can use
    /// entry names the writer would sanitize away, such as `../` traversal.
    fn build_raw_zip(archive_path: &Path, entry_name: &str, payload: &[u8]) {
        use std::io::Write;

        let crc = crc32_ieee(payload);
        let name = entry_name.as_bytes();

        let mut bytes = Vec::new();
        // Local file header.
        bytes.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        bytes.extend_from_slice(&20u16.to_le_bytes()); // version needed
        bytes.extend_from_slice(&0u16.to_le_bytes()); // flags
        bytes.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        bytes.extend_from_slice(&0u16.to_le_bytes()); // mod time
        bytes.extend_from_slice(&0x21u16.to_le_bytes()); // mod date (1980-01-01)
        bytes.extend_from_slice(&crc.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes()); // extra length
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(payload);

        let mut central = Vec::new();
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&0x031eu16.to_le_bytes()); // version made by (unix)
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0u16.to_le_bytes()); // flags
        central.extend_from_slice(&0u16.to_le_bytes()); // method
        central.extend_from_slice(&0u16.to_le_bytes()); // mod time
        central.extend_from_slice(&0x21u16.to_le_bytes()); // mod date
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        central.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra length
        central.extend_from_slice(&0u16.to_le_bytes()); // comment length
        central.extend_from_slice(&0u16.to_le_bytes()); // disk number start
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
        central.extend_from_slice(&(0o100644u32 << 16).to_le_bytes()); // external attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // local header offset
        central.extend_from_slice(name);

        let mut eocd = Vec::new();
        eocd.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        eocd.extend_from_slice(&0u16.to_le_bytes()); // this disk
        eocd.extend_from_slice(&0u16.to_le_bytes()); // cd disk
        eocd.extend_from_slice(&1u16.to_le_bytes()); // entries on this disk
        eocd.extend_from_slice(&1u16.to_le_bytes()); // total entries
        eocd.extend_from_slice(&(central.len() as u32).to_le_bytes());
        eocd.extend_from_slice(&(bytes.len() as u32).to_le_bytes()); // cd offset
        eocd.extend_from_slice(&0u16.to_le_bytes()); // comment length

        bytes.extend_from_slice(&central);
        bytes.extend_from_slice(&eocd);

        let mut file = File::create(archive_path).unwrap();
        file.write_all(&bytes).unwrap();
    }

    /// Standard CRC-32 (IEEE 802.3) for hand-built ZIP fixtures.
    fn crc32_ieee(data: &[u8]) -> u32 {
        let mut table = [0u32; 256];
        for (index, entry) in table.iter_mut().enumerate() {
            let mut value = index as u32;
            for _ in 0..8 {
                value = if value & 1 != 0 {
                    0xedb8_8320 ^ (value >> 1)
                } else {
                    value >> 1
                };
            }
            *entry = value;
        }
        let mut crc = 0xffff_ffffu32;
        for &byte in data {
            crc = table[((crc ^ byte as u32) & 0xff) as usize] ^ (crc >> 8);
        }
        !crc
    }

    #[test]
    fn zip_extracts_directory_entries_and_files() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let temp_dir = tempdir().unwrap();
        let archive_path = temp_dir.path().join("fixture.zip");
        build_zip_archive(&archive_path, |writer| {
            writer
                .add_directory("pkg/", SimpleFileOptions::default())
                .unwrap();
            writer
                .start_file("pkg/bin/tool", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"#!/bin/sh\n").unwrap();
        });

        let destination = temp_dir.path().join("out");
        extract_zip(&archive_path, &destination, &AtomicBool::new(false)).unwrap();

        assert!(destination.join("pkg/bin").is_dir());
        assert!(destination.join("pkg/bin/tool").is_file());
        assert_eq!(
            std::fs::read(destination.join("pkg/bin/tool")).unwrap(),
            b"#!/bin/sh\n"
        );
    }

    #[test]
    fn zip_rejects_path_traversal_entries() {
        let temp_dir = tempdir().unwrap();
        let archive_path = temp_dir.path().join("escape.zip");
        build_raw_zip(&archive_path, "../escape.txt", b"evil");

        let destination = temp_dir.path().join("out");
        assert!(extract_zip(&archive_path, &destination, &AtomicBool::new(false)).is_err());
        assert!(!destination.join("escape.txt").exists());
        assert!(!temp_dir.path().join("escape.txt").exists());
    }

    #[test]
    fn zip_extracts_symlinks_as_symlinks() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let temp_dir = tempdir().unwrap();
        let archive_path = temp_dir.path().join("fixture.zip");
        build_zip_archive(&archive_path, |writer| {
            writer
                .start_file("pkg/target", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"payload").unwrap();
            writer
                .add_symlink("pkg/link", "target", SimpleFileOptions::default())
                .unwrap();
        });

        let destination = temp_dir.path().join("out");
        extract_zip(&archive_path, &destination, &AtomicBool::new(false)).unwrap();

        let link = destination.join("pkg/link");
        let link_metadata = std::fs::symlink_metadata(&link).unwrap();
        assert!(link_metadata.file_type().is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), PathBuf::from("target"));
    }

    #[test]
    fn zip_preserves_unix_permissions_and_executable_helpers() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use zip::write::SimpleFileOptions;

        let temp_dir = tempdir().unwrap();
        let archive_path = temp_dir.path().join("fixture.zip");
        build_zip_archive(&archive_path, |writer| {
            writer
                .start_file(
                    "pkg/run.sh",
                    SimpleFileOptions::default().unix_permissions(0o755),
                )
                .unwrap();
            writer.write_all(b"#!/bin/sh\n").unwrap();
            writer
                .start_file(
                    "pkg/README",
                    SimpleFileOptions::default().unix_permissions(0o644),
                )
                .unwrap();
            writer.write_all(b"readme").unwrap();
        });

        let destination = temp_dir.path().join("out");
        extract_zip(&archive_path, &destination, &AtomicBool::new(false)).unwrap();

        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&destination.join("pkg/run.sh")), 0o755);
        assert_eq!(mode(&destination.join("pkg/README")), 0o644);
    }

    // --- Cancellation and stale-entry recovery ---

    /// Serves `body` over plain HTTP on an ephemeral localhost port and returns
    /// its URL. With `dribble`, the body is sent one byte at a time with a
    /// pause between bytes, so a client stays suspended mid-download until the
    /// test aborts it.
    async fn serve_archive(body: Vec<u8>, dribble: Option<Duration>, file_name: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(headers.as_bytes()).await;
            match dribble {
                Some(interval) => {
                    for byte in body {
                        let _ = socket.write_all(&[byte]).await;
                        tokio::time::sleep(interval).await;
                    }
                }
                None => {
                    let _ = socket.write_all(&body).await;
                }
            }
        });
        format!("http://{address}/{file_name}")
    }

    async fn serve_archive_without_length(body: Vec<u8>, file_name: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .await;
            let _ = socket.write_all(&body).await;
        });
        format!("http://{address}/{file_name}")
    }

    async fn serve_stalled_archive(content_length: usize, file_name: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
            );
            let _ = socket.write_all(headers.as_bytes()).await;
            let _ = socket.write_all(b"x").await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        format!("http://{address}/{file_name}")
    }

    /// Builds a ZIP archive with `entry_count` stored (uncompressed) entries.
    /// Building stored entries is cheap in debug builds, while extracting tens
    /// of thousands of them takes seconds, so a cancellation can be observed
    /// deterministically mid-extraction.
    fn many_entries_zip(entry_count: u32) -> Vec<u8> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let mut bytes = Vec::new();
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for index in 0..entry_count {
            writer
                .start_file(format!("pkg/f{index:05}.bin"), options)
                .unwrap();
            writer.write_all(&[index as u8; 64]).unwrap();
        }
        writer.finish().unwrap();
        bytes
    }

    #[test]
    fn zip_rejects_expanded_size_and_entry_quotas() {
        let temp_dir = tempdir().unwrap();
        let archive_path = temp_dir.path().join("fixture.zip");
        std::fs::write(&archive_path, many_entries_zip(3)).unwrap();
        let base = ArchiveLimits {
            max_download_bytes: 1024 * 1024,
            max_expanded_bytes: 128,
            max_entries: 10,
            max_files: 10,
            ..ArchiveLimits::default()
        };
        let error = extract_zip_with_limits(
            &archive_path,
            &temp_dir.path().join("out-size"),
            &AtomicBool::new(false),
            base,
        )
        .unwrap_err();
        assert!(error.to_string().contains("expanded archive size limit"));

        let error = extract_zip_with_limits(
            &archive_path,
            &temp_dir.path().join("out-entries"),
            &AtomicBool::new(false),
            ArchiveLimits {
                max_entries: 2,
                ..base
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("entry limit"));
    }

    #[test]
    fn compressed_tar_and_zip_reject_expanded_size_bombs() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use zip::write::SimpleFileOptions;

        let temp_dir = tempdir().unwrap();
        let payload = vec![0u8; 64 * 1024];
        let limits = ArchiveLimits {
            max_expanded_bytes: 1024,
            ..ArchiveLimits::default()
        };

        let tar_path = temp_dir.path().join("bomb.tar.gz");
        let encoder = GzEncoder::new(File::create(&tar_path).unwrap(), Compression::best());
        let mut tar = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_path("pkg/payload.bin").unwrap();
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, payload.as_slice()).unwrap();
        tar.into_inner().unwrap().finish().unwrap();

        let error = extract_archive_blocking(
            &tar_path,
            &temp_dir.path().join("tar-out"),
            &AtomicBool::new(false),
            limits,
        )
        .unwrap_err();
        assert!(error.to_string().contains("expanded archive size limit"));

        let zip_path = temp_dir.path().join("bomb.zip");
        build_zip_archive(&zip_path, |writer| {
            writer
                .start_file(
                    "pkg/payload.bin",
                    SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated),
                )
                .unwrap();
            writer.write_all(&payload).unwrap();
        });
        let error = extract_zip_with_limits(
            &zip_path,
            &temp_dir.path().join("zip-out"),
            &AtomicBool::new(false),
            limits,
        )
        .unwrap_err();
        assert!(error.to_string().contains("expanded archive size limit"));
    }

    #[tokio::test]
    async fn download_rejects_oversized_response_before_writing() {
        let temp_dir = tempdir().unwrap();
        let body = vec![0x42; 32];
        let digest = hex_encode(Sha256::digest(&body).as_slice());
        let url = serve_archive(body, None, "payload.zip").await;
        let target = BinaryTarget {
            archive: url,
            cmd: "tool".to_string(),
            sha256: Some(digest),
            args: None,
            env: None,
        };
        let error = download_archive_with_limits(
            &target,
            temp_dir.path(),
            ArchiveLimits {
                max_download_bytes: 16,
                ..ArchiveLimits::default()
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("download limit"));
        assert!(!temp_dir.path().join("payload.zip").exists());
    }

    #[tokio::test]
    async fn download_enforces_limit_without_content_length() {
        let temp_dir = tempdir().unwrap();
        let body = vec![0x42; 32];
        let digest = hex_encode(Sha256::digest(&body).as_slice());
        let target = BinaryTarget {
            archive: serve_archive_without_length(body, "payload.zip").await,
            cmd: "tool".to_string(),
            sha256: Some(digest),
            args: None,
            env: None,
        };

        let error = download_archive_with_limits(
            &target,
            temp_dir.path(),
            ArchiveLimits {
                max_download_bytes: 16,
                ..ArchiveLimits::default()
            },
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("download limit"));
        assert!(!temp_dir.path().join("payload.zip").exists());
    }

    #[tokio::test]
    async fn download_read_timeout_removes_partial_file() {
        let temp_dir = tempdir().unwrap();
        let target = BinaryTarget {
            archive: serve_stalled_archive(32, "payload.zip").await,
            cmd: "tool".to_string(),
            sha256: Some(hex_encode(Sha256::digest([0u8; 32]).as_slice())),
            args: None,
            env: None,
        };

        let error = download_archive_with_limits(
            &target,
            temp_dir.path(),
            ArchiveLimits {
                read_timeout: Duration::from_millis(50),
                total_timeout: Duration::from_secs(2),
                ..ArchiveLimits::default()
            },
        )
        .await
        .unwrap_err();

        assert!(
            format!("{error:#}").contains("failed to read archive response"),
            "unexpected timeout error: {error:#}"
        );
        assert!(!temp_dir.path().join("payload.zip").exists());
    }

    /// Dot-prefixed entry names directly inside `dir` (staging/backup work
    /// directories left behind by interrupted installs).
    async fn work_dirs_in(dir: &Path) -> Vec<String> {
        let mut names = Vec::new();
        let Ok(mut entries) = fs::read_dir(dir).await else {
            return names;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                names.push(name);
            }
        }
        names
    }

    /// Waits until the in-flight install has started writing extracted entries
    /// into its staging directory, then returns, so a test can land a
    /// cancellation mid-extraction regardless of machine speed.
    async fn wait_for_extraction_started(parent_dir: &Path) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let Ok(mut entries) = fs::read_dir(parent_dir).await else {
                continue;
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.starts_with('.') {
                    continue;
                }
                let Ok(mut extracted_entries) = fs::read_dir(entry.path().join("extracted")).await
                else {
                    continue;
                };
                if let Ok(Some(_)) = extracted_entries.next_entry().await {
                    return;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "extraction never started"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn binary_test_agent(version: &str, archive_url: &str) -> RegistryAgent {
        let binary = BinaryDistribution {
            linux_x86_64: Some(BinaryTarget {
                archive: archive_url.to_string(),
                cmd: "./bin/demo".to_string(),
                sha256: None,
                args: None,
                env: None,
            }),
            ..BinaryDistribution::default()
        };
        RegistryAgent {
            id: "demo".to_string(),
            name: "Demo".to_string(),
            version: version.to_string(),
            description: "Demo agent".to_string(),
            repository: None,
            website: None,
            authors: vec!["ACP".to_string()],
            license: "MIT".to_string(),
            icon: None,
            distribution: AgentDistribution {
                binary: Some(binary),
                npx: None,
                uvx: None,
            },
        }
    }

    #[tokio::test]
    async fn malformed_digest_is_rejected_before_cache_path_creation() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache");
        let platform = Platform::LinuxX86_64;
        let agent = binary_test_agent("1.0.0", "not a valid URL");
        let mut target = agent
            .distribution
            .binary
            .as_ref()
            .unwrap()
            .for_platform(platform)
            .unwrap()
            .clone();
        target.sha256 = Some("../../not-a-digest".to_string());

        let error = cache_binary_target_in_mode(&cache_root, &agent, platform, &target, false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("invalid sha256 checksum"));
        assert!(!cache_root.exists());
    }

    #[tokio::test]
    async fn cancelled_download_removes_the_staging_directory() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let platform = Platform::LinuxX86_64;
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", platform);

        // The archive dribbles out over minutes, so the install is guaranteed
        // to still be awaiting the download when the task is aborted.
        let body = vec![0x42; 64 * 1024];
        let digest = hex_encode(Sha256::digest(&body).as_slice());
        let url = serve_archive(body, Some(Duration::from_millis(25)), "agent.tar.gz").await;
        let agent = binary_test_agent("1.0.0", &url);
        let mut target = agent
            .distribution
            .binary
            .as_ref()
            .unwrap()
            .for_platform(platform)
            .unwrap()
            .clone();
        target.sha256 = Some(digest);

        let root = cache_root.clone();
        let task = tokio::spawn(async move {
            cache_binary_target_in_mode(&root, &agent, platform, &target, false).await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        task.abort();
        assert!(task.await.is_err(), "install should have been cancelled");

        assert!(!paths.cache_dir.exists(), "no cache may be promoted");
        assert_eq!(
            work_dirs_in(&paths.parent_dir).await,
            Vec::<String>::new(),
            "staging directory must be removed on cancellation"
        );
    }

    #[tokio::test]
    async fn cancelled_extraction_removes_the_staging_directory() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let platform = Platform::LinuxX86_64;
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", platform);

        // Stay below the production entry quota while still making extraction
        // long enough for cancellation to land between entries.
        let body = many_entries_zip(8_000);
        let digest = hex_encode(Sha256::digest(&body).as_slice());
        let url = serve_archive(body, None, "agent.zip").await;
        let agent = binary_test_agent("1.0.0", &url);
        let mut target = agent
            .distribution
            .binary
            .as_ref()
            .unwrap()
            .for_platform(platform)
            .unwrap()
            .clone();
        target.sha256 = Some(digest);

        let root = cache_root.clone();
        let task = tokio::spawn(async move {
            cache_binary_target_in_mode(&root, &agent, platform, &target, false).await
        });
        wait_for_extraction_started(&paths.parent_dir).await;
        task.abort();
        assert!(task.await.is_err(), "install should have been cancelled");

        // The detached blocking extraction keeps running until its next write
        // fails against the removed directory; give it a moment to wind down
        // before asserting the staging directory is gone.
        for _ in 0..100 {
            if work_dirs_in(&paths.parent_dir).await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!paths.cache_dir.exists(), "no cache may be promoted");
        assert_eq!(
            work_dirs_in(&paths.parent_dir).await,
            Vec::<String>::new(),
            "staging directory must be removed on cancellation"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_publication_wait_removes_prepared_staging() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths_with_digest(
            &cache_root,
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            &"a".repeat(64),
        );
        fs::create_dir_all(&paths.parent_dir).await.unwrap();
        let staging_path = paths.parent_dir.join(".prepared-staging-cancel");
        fs::create_dir_all(&staging_path).await.unwrap();
        fs::write(staging_path.join("payload"), b"prepared")
            .await
            .unwrap();

        // Hold a reader so the publisher blocks while waiting for the exact
        // use-write lease acquired by the production path.
        let held_reader = acquire_binary_cache_use_read_lock(&paths).await.unwrap();
        let publication_lock = Arc::new(acquire_binary_cache_lock(&paths).await.unwrap());
        let task_paths = paths.clone();
        let task_staging = PreparedStaging::new(staging_path.clone(), publication_lock);
        let task = tokio::spawn(async move {
            let _writer = acquire_binary_cache_use_write_lock(&task_paths)
                .await
                .unwrap();
            let expected = BinaryCacheMetadata::new(
                "demo",
                "1.0.0",
                Platform::LinuxX86_64,
                "https://example.com/demo.tar.gz",
                "./demo",
                Some(&"a".repeat(64)),
            );
            promote_prepared_cache(task_staging, &task_paths, &expected, false, None).await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        task.abort();
        assert!(task.await.is_err(), "publication wait should be cancelled");
        drop(held_reader);

        for _ in 0..100 {
            if !staging_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !staging_path.exists(),
            "prepared staging must be cleaned up"
        );
        tokio::time::timeout(
            Duration::from_secs(2),
            acquire_binary_cache_use_write_lock(&paths),
        )
        .await
        .expect("cancelled publication must release its use lock")
        .unwrap();
    }

    #[tokio::test]
    async fn startup_sweep_removes_stale_staging_and_backup_directories() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);

        // A completed cache entry must survive the sweep.
        fs::create_dir_all(&paths.extracted_dir).await.unwrap();
        fs::write(&paths.metadata_path, b"{}").await.unwrap();

        // Work directories abandoned by installs that were killed mid-flight.
        let stale_staging = paths.parent_dir.join(".1.0.0-staging-123-456");
        let stale_backup = paths.parent_dir.join(".1.0.0-backup-123-456");
        fs::create_dir_all(stale_staging.join("extracted"))
            .await
            .unwrap();
        fs::create_dir_all(&stale_backup).await.unwrap();

        let removed = clean_stale_staging_entries_in(&cache_root).await;

        assert_eq!(removed, 2);
        assert!(!stale_staging.exists());
        assert!(!stale_backup.exists());
        assert!(paths.cache_dir.exists());
        assert!(paths.metadata_path.exists());
    }

    #[tokio::test]
    async fn startup_sweep_restores_backup_when_final_cache_is_missing() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        let backup = paths.parent_dir.join(".1.0.0-backup-crashed");
        fs::create_dir_all(backup.join(EXTRACTED_DIR_NAME))
            .await
            .unwrap();
        fs::write(backup.join(METADATA_FILE_NAME), b"{\"recovered\":true}")
            .await
            .unwrap();

        let removed = clean_stale_staging_entries_in(&cache_root).await;

        assert_eq!(removed, 0);
        assert!(paths.cache_dir.exists());
        assert!(!backup.exists());
        assert_eq!(
            fs::read(paths.metadata_path).await.unwrap(),
            b"{\"recovered\":true}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_sweep_skips_active_staging_lock() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        fs::create_dir_all(&paths.parent_dir).await.unwrap();
        let active_staging = paths.parent_dir.join(".1.0.0-staging-active");
        fs::create_dir_all(&active_staging).await.unwrap();

        let lock = acquire_binary_cache_lock(&paths).await.unwrap();
        let removed = tokio::time::timeout(
            Duration::from_secs(1),
            clean_stale_staging_entries_in(&cache_root),
        )
        .await
        .expect("startup sweep must not wait for an active installer");
        assert_eq!(removed, 0);
        assert!(active_staging.exists());

        drop(lock);
        assert_eq!(clean_stale_staging_entries_in(&cache_root).await, 1);
        assert!(!active_staging.exists());
    }
}

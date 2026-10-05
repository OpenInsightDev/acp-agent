//! End-to-end tests for [`InstallBinary.md`](InstallBinary.md).
//!
//! Every case runs the CLI against the checked-in mock catalog
//! (`tests/fixtures/registry.json`) served by the harness. That catalog carries
//! two placeholders in its binary targets: `{fixture_server}` becomes the
//! fixture server's URL, and `{archive_sha256}` becomes the digest of the
//! archive the case built. `MockCatalog::rendered` resolves them, so a case can
//! pin the exact archive URL, digest, cache path, and file contents it expects.
//!
//! Cases build archive bytes with `binary_archive` (or its escaping-entry and
//! many-directory variants), write them below the harness fixture directory with
//! `write_catalog_archives`, and observe the cache through the published paths,
//! `list --installed`, and the fixture server's request counts.
//!
//! The two recovery cases seed their own work directories beside a
//! `seed_cached_binary` entry on a platform other than the host's.

mod harness;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use harness::{Harness, MockCatalog, binary_archive, host_platform_key, write_catalog_archives};

const AGENT_ID: &str = "mock-binary";
const AGENT_VERSION: &str = "2.3.4";
const CMD: &str = "bin/mock-binary";
const SCRIPT: &str = "#!/bin/sh\n# fixture binary\nprintf 'ok\\n'\n";
const REPLACEMENT_SCRIPT: &str = "#!/bin/sh\n# replacement\nprintf 'new\\n'\n";

/// `ArchiveLimits::max_entries` for the shipped defaults; an archive must stay
/// under it, directories included.
const MAX_ENTRIES: usize = 10_000;

fn archive() -> Vec<u8> {
    binary_archive(CMD, SCRIPT, &[("lib/notice.txt", "fixture distribution")])
}

/// Writes the archive the mock catalog serves and returns the rendered catalog
/// whose host `archive` URL and `sha256` name exactly those bytes.
async fn catalog_with(harness: &Harness, bytes: &[u8]) -> MockCatalog {
    write_catalog_archives(harness, bytes);
    MockCatalog::rendered(harness).await
}

fn agent_dir(harness: &Harness) -> PathBuf {
    harness.cache_root().join("agents").join(AGENT_ID)
}

fn platform_dir(harness: &Harness) -> PathBuf {
    agent_dir(harness).join(host_platform_key())
}

fn cache_dir(harness: &Harness, digest: &str) -> PathBuf {
    platform_dir(harness).join(format!("{AGENT_VERSION}-sha256-{digest}"))
}

fn child_names(dir: &Path) -> Vec<String> {
    let mut names = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", dir.display()))
        .map(|entry| entry.expect("a directory entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// Dot-prefixed staging and backup directories under the host platform's cache
/// parent, which no completed install should leave behind.
fn work_dirs(harness: &Harness) -> Vec<PathBuf> {
    let dir = platform_dir(harness);
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut dirs = fs::read_dir(&dir)
        .expect("the platform directory is readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with('.'))
        })
        .collect::<Vec<_>>();
    dirs.sort();
    dirs
}

async fn installed(harness: &Harness) -> Vec<serde_json::Value> {
    harness
        .run(&["list", "--installed", "--json"])
        .await
        .success()
        .json()
        .as_array()
        .expect("list --installed --json prints an array")
        .clone()
}

fn metadata(cache_dir: &Path) -> serde_json::Value {
    let bytes = fs::read(cache_dir.join("metadata.json")).expect("the manifest is readable");
    serde_json::from_slice(&bytes).expect("the manifest is JSON")
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .expect("the executable exists")
        .permissions()
        .mode()
        & 0o111
        != 0
}

mod install {
    use std::fs;

    use crate::harness::{Harness, catalog_archive_path, host_platform_key, sha256_hex};
    use crate::{
        AGENT_ID, AGENT_VERSION, CMD, SCRIPT, agent_dir, archive, cache_dir, catalog_with,
        child_names, installed, is_executable, metadata, platform_dir,
    };

    #[tokio::test]
    async fn selects_host_target() {
        let harness = Harness::new();
        let bytes = archive();
        let catalog = catalog_with(&harness, &bytes).await;
        catalog
            .run(&harness, &["install", AGENT_ID])
            .await
            .success();

        // Only the host platform's directory is materialized, even though the
        // catalog declares all four.
        assert_eq!(child_names(&agent_dir(&harness)), vec![host_platform_key()]);

        // The manifest names the host platform's archive URL, not another's.
        let manifest = metadata(&cache_dir(&harness, &sha256_hex(&bytes)));
        let expected_archive = format!(
            "{}/{}",
            catalog.base_url(),
            catalog_archive_path(host_platform_key())
        );
        assert_eq!(manifest["platform"], host_platform_key());
        assert_eq!(manifest["archive"], expected_archive);
    }

    #[tokio::test]
    async fn publishes_digest_keyed_cache() {
        let harness = Harness::new();
        let bytes = archive();
        let digest = sha256_hex(&bytes);
        let catalog = catalog_with(&harness, &bytes).await;
        let output = catalog.run(&harness, &["install", AGENT_ID]).await;
        output.success();

        let dir = cache_dir(&harness, &digest);
        let executable = dir.join("extracted").join(CMD);
        assert!(executable.is_file());
        assert!(is_executable(&executable));
        assert_eq!(fs::read_to_string(&executable).unwrap(), SCRIPT);

        let manifest = metadata(&dir);
        assert_eq!(manifest["agent_id"], AGENT_ID);
        assert_eq!(manifest["agent_version"], AGENT_VERSION);
        assert_eq!(manifest["sha256"], digest);
        assert!(manifest["executable_sha256"].is_string());
        assert!(manifest["payload_sha256"].is_string());

        assert!(
            output.stdout.contains(&format!(
                "Installed {AGENT_ID} binary at {}",
                executable.display()
            )),
            "{}",
            output.describe()
        );
    }

    #[tokio::test]
    async fn warm_cache_reuses_archive() {
        let harness = Harness::new();
        let bytes = archive();
        let catalog = catalog_with(&harness, &bytes).await;
        let relative = catalog_archive_path(host_platform_key());

        catalog
            .run(&harness, &["install", AGENT_ID])
            .await
            .success();
        assert_eq!(catalog.request_count(&relative), 1);

        catalog
            .run(&harness, &["install", AGENT_ID])
            .await
            .success();
        assert_eq!(catalog.request_count(&relative), 1);
        assert!(cache_dir(&harness, &sha256_hex(&bytes)).is_dir());
    }

    #[tokio::test]
    async fn lists_installed_agents() {
        let harness = Harness::new();
        let bytes = archive();
        let catalog = catalog_with(&harness, &bytes).await;
        catalog
            .run(&harness, &["install", AGENT_ID])
            .await
            .success();

        let dir = cache_dir(&harness, &sha256_hex(&bytes));
        let executable = dir.join("extracted").join(CMD);

        let records = installed(&harness).await;
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record["id"], AGENT_ID);
        assert_eq!(record["version"], AGENT_VERSION);
        assert_eq!(record["platform"], host_platform_key());
        assert_eq!(record["cache_dir"], dir.display().to_string());
        assert_eq!(record["executable_path"], executable.display().to_string());

        let tsv = harness
            .run(&["list", "--installed"])
            .await
            .success()
            .stdout
            .clone();
        assert_eq!(
            tsv,
            format!(
                "{AGENT_ID}\t{AGENT_VERSION}\t{}\t{}\n",
                host_platform_key(),
                dir.display()
            )
        );
        assert!(platform_dir(&harness).is_dir());
    }
}

mod verify {
    use crate::harness::{Harness, binary_archive, write_catalog_archives};
    use crate::{AGENT_ID, CMD, archive, catalog_with, installed, work_dirs};

    #[tokio::test]
    async fn rejects_checksum_mismatch() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &archive()).await;

        // Swap the served bytes after the catalog declared their digest, so the
        // download no longer matches.
        write_catalog_archives(
            &harness,
            &binary_archive(CMD, "#!/bin/sh\nprintf 'tampered\\n'\n", &[]),
        );

        let output = catalog.run(&harness, &["install", AGENT_ID]).await;
        assert_eq!(output.failure().exit_code(), 1);
        output.stdout_contains("sha256 checksum mismatch");
        assert!(installed(&harness).await.is_empty());
        assert!(work_dirs(&harness).is_empty());
    }
}

mod archive {
    use crate::harness::{
        Harness, binary_archive_with_empty_directories, binary_archive_with_escaping_entry,
    };
    use crate::{AGENT_ID, CMD, MAX_ENTRIES, SCRIPT, catalog_with, installed, work_dirs};

    #[tokio::test]
    async fn rejects_path_traversal() {
        let harness = Harness::new();
        let bytes = binary_archive_with_escaping_entry(CMD, SCRIPT, "../escape.txt");
        let catalog = catalog_with(&harness, &bytes).await;

        let output = catalog.run(&harness, &["install", AGENT_ID]).await;
        assert_eq!(output.failure().exit_code(), 1);
        output.stdout_contains("unsafe archive path");
        assert!(installed(&harness).await.is_empty());
        assert!(work_dirs(&harness).is_empty());
    }

    #[tokio::test]
    async fn rejects_entry_limit() {
        let harness = Harness::new();
        // One executable plus MAX_ENTRIES directory entries is one entry too
        // many, while keeping the non-directory count under the file limit.
        let bytes = binary_archive_with_empty_directories(CMD, SCRIPT, MAX_ENTRIES);
        let catalog = catalog_with(&harness, &bytes).await;

        let output = catalog.run(&harness, &["install", AGENT_ID]).await;
        assert_eq!(output.failure().exit_code(), 1);
        output.stdout_contains("archive entry limit exceeded");
        assert!(installed(&harness).await.is_empty());
        assert!(work_dirs(&harness).is_empty());
    }
}

mod update {
    use crate::harness::{Harness, binary_archive, sha256_hex, write_catalog_archives};
    use crate::{AGENT_ID, CMD, REPLACEMENT_SCRIPT, archive, cache_dir, catalog_with};

    #[tokio::test]
    async fn installs_replacement() {
        let harness = Harness::new();
        let first = archive();
        let catalog = catalog_with(&harness, &first).await;
        catalog
            .run(&harness, &["install", AGENT_ID])
            .await
            .success();
        let first_dir = cache_dir(&harness, &sha256_hex(&first));
        assert!(first_dir.is_dir());

        // The catalog now publishes a different digest for the same version.
        let second = binary_archive(CMD, REPLACEMENT_SCRIPT, &[]);
        write_catalog_archives(&harness, &second);
        catalog.rerender(&harness);
        let output = catalog.run(&harness, &["update", AGENT_ID]).await;
        output.success();

        let second_dir = cache_dir(&harness, &sha256_hex(&second));
        assert_ne!(first_dir, second_dir);
        assert_eq!(
            std::fs::read_to_string(second_dir.join("extracted").join(CMD)).unwrap(),
            REPLACEMENT_SCRIPT
        );
        assert!(
            output
                .stdout
                .contains(&format!("Installed {AGENT_ID} binary at")),
            "{}",
            output.describe()
        );
    }

    #[tokio::test]
    async fn removes_replaced() {
        let harness = Harness::new();
        let first = archive();
        let catalog = catalog_with(&harness, &first).await;
        catalog
            .run(&harness, &["install", AGENT_ID])
            .await
            .success();
        let replaced_dir = cache_dir(&harness, &sha256_hex(&first));
        assert!(replaced_dir.is_dir());

        let second = binary_archive(CMD, REPLACEMENT_SCRIPT, &[]);
        write_catalog_archives(&harness, &second);
        catalog.rerender(&harness);
        catalog.run(&harness, &["update", AGENT_ID]).await.success();

        assert!(
            !replaced_dir.is_dir(),
            "the replaced entry is removed: {}",
            replaced_dir.display()
        );
        let replacement = cache_dir(&harness, &sha256_hex(&second));
        let records = catalog
            .run(&harness, &["list", "--installed", "--json"])
            .await
            .success()
            .json();
        let records = records
            .as_array()
            .expect("list --installed --json prints an array");
        assert_eq!(records.len(), 1, "only one entry remains: {records:?}");
        assert_eq!(
            records[0]["cache_dir"]
                .as_str()
                .expect("cache_dir is a string"),
            replacement.display().to_string()
        );
    }
}

mod uninstall {
    use crate::harness::{Harness, sha256_hex};
    use crate::{AGENT_ID, archive, cache_dir, catalog_with, installed};

    #[tokio::test]
    async fn removes_cached_entry() {
        let harness = Harness::new();
        let bytes = archive();
        let catalog = catalog_with(&harness, &bytes).await;
        catalog
            .run(&harness, &["install", AGENT_ID])
            .await
            .success();
        let dir = cache_dir(&harness, &sha256_hex(&bytes));
        assert!(dir.is_dir());

        catalog
            .run(&harness, &["uninstall", AGENT_ID])
            .await
            .success()
            .stdout_contains(&format!("Uninstalled {AGENT_ID} from the local cache"));
        assert!(!dir.exists());
        assert!(installed(&harness).await.is_empty());
    }

    #[tokio::test]
    async fn rejects_unknown_agent() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &archive()).await;

        let output = catalog.run(&harness, &["uninstall", "mock-absent"]).await;
        assert_eq!(output.failure().exit_code(), 1);
        output.stdout_contains("is not installed");
    }
}

mod recovery {
    use std::fs;

    use crate::harness::{Harness, other_platform_key, seed_cached_binary};

    #[tokio::test]
    async fn sweeps_stale_staging() {
        let harness = Harness::new();
        // A non-host platform proves the sweep covers every platform.
        let seeded = seed_cached_binary(
            &harness,
            "fixture-agent",
            "1.2.3",
            other_platform_key(),
            "#!/bin/sh\nexit 0\n",
        );
        let parent = seeded.cache_dir.parent().expect("the entry has a parent");
        let staging = parent.join(".9.9.9-sha256-deadbeef-staging-1-1");
        fs::create_dir_all(staging.join("extracted")).unwrap();

        // Any command sweeps at startup.
        harness.run(&["list", "--installed"]).await.success();

        assert!(!staging.exists());
        assert!(seeded.cache_dir.is_dir());
    }

    #[tokio::test]
    async fn restores_backup() {
        let harness = Harness::new();
        let parent = harness
            .cache_root()
            .join("agents")
            .join("fixture-agent")
            .join(other_platform_key());
        fs::create_dir_all(&parent).unwrap();
        let entry = parent.join("1.2.3-sha256-deadbeef");
        let backup = parent.join(".1.2.3-sha256-deadbeef-backup-1-1");
        fs::create_dir_all(backup.join("extracted")).unwrap();
        fs::write(backup.join("metadata.json"), r#"{"recovered":true}"#).unwrap();

        harness.run(&["list", "--installed"]).await.success();

        assert!(entry.is_dir());
        assert!(!backup.exists());
        assert_eq!(
            fs::read_to_string(entry.join("metadata.json")).unwrap(),
            r#"{"recovered":true}"#
        );
    }
}

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};

use super::{Harness, platform_keys};

/// A fixture ACP agent: a shell script speaking minimal JSON-RPC over stdio.
#[derive(Debug, Clone)]
pub enum AcpAgent {
    /// Answers `initialize` and `test/echo`.
    Echo,
    /// Stands in for an agent that fails to start.
    Fail { code: i32, stderr: String },
    /// Stands in for an agent that hangs.
    Silent,
}

impl AcpAgent {
    pub fn install(&self, harness: &Harness, name: &str) -> PathBuf {
        harness.write_script(name, &self.script())
    }

    pub fn script(&self) -> String {
        match self {
            Self::Echo => {
                r#"#!/bin/sh
# Fixture ACP agent: replies to initialize and test/echo with the request id.
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  [ -n "$id" ] || id=0
  case "$line" in
  *'"method":"initialize"'*)
    printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{}}}"
    ;;
  *'"method":"test/echo"'*)
    printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"echo\":\"ok\"}}"
    ;;
  *)
    printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{\"code\":-32601,\"message\":\"method not found\"}}"
    ;;
  esac
done
"#
                .to_string()
            }
            Self::Fail { code, stderr } => format!(
                "#!/bin/sh\nprintf '%s\\n' {} >&2\nexit {code}\n",
                shell_quote(stderr)
            ),
            Self::Silent => "#!/bin/sh\n# Fixture ACP agent: stays alive without ever answering.\nwhile IFS= read -r line; do :; done\n".to_string(),
        }
    }
}

/// One recorded invocation of a [`fake_program`].
#[derive(Debug, PartialEq, Eq)]
pub struct FakeInvocation {
    /// Arguments the caller passed, in order.
    pub args: Vec<String>,
    /// Value of the mock catalog's `MOCK_MODE` variable the process inherited, or
    /// `None` when no such variable reached it.
    pub mock_mode: Option<String>,
}

/// Writes a fake package manager (`npm`, `deno`, `uv`, or `uvx`) into the harness
/// `bin/` directory, where it shadows the real tool on `PATH`.
///
/// Each invocation appends a record to `log`: the argument count, one line per
/// argument, then the inherited `MOCK_MODE` value; the program then exits
/// successfully. When `FAKE_LIST_OUTPUT` is set and the first argument is
/// `list`, it prints that value to standard output, standing in for npm's
/// `npm list --json` global inventory. Read the records back with
/// [`fake_invocations`].
pub fn fake_program(harness: &Harness, name: &str, log: &Path) -> PathBuf {
    let body = format!(
        "#!/bin/sh\n{{\n  printf '%s\\n' \"$#\"\n  for arg in \"$@\"; do printf '%s\\n' \"$arg\"; done\n  printf '%s\\n' \"${{MOCK_MODE-}}\"\n}} >> {log}\nif [ \"$1\" = \"list\" ] && [ -n \"${{FAKE_LIST_OUTPUT-}}\" ]; then\n  printf '%s\\n' \"$FAKE_LIST_OUTPUT\"\nfi\nexit 0\n",
        log = shell_quote(&log.display().to_string()),
    );
    harness.write_script(name, &body)
}

/// Reads the records a [`fake_program`] appended to `log`.
///
/// A program that was never invoked (so left no log file) records nothing.
pub fn fake_invocations(log: &Path) -> Vec<FakeInvocation> {
    let contents = match fs::read_to_string(log) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("failed to read {}: {error}", log.display()),
    };
    let mut lines = contents.lines();
    let mut invocations = Vec::new();
    while let Some(count) = lines.next() {
        let count: usize = count
            .parse()
            .unwrap_or_else(|error| panic!("malformed fake program log ({count:?}): {error}"));
        let args = (0..count)
            .map(|_| {
                lines
                    .next()
                    .expect("fake program log has one line per argument")
                    .to_string()
            })
            .collect();
        let mock_mode = lines.next().expect("fake program log records MOCK_MODE");
        invocations.push(FakeInvocation {
            args,
            mock_mode: (!mock_mode.is_empty()).then(|| mock_mode.to_string()),
        });
    }
    invocations
}

#[derive(Debug, Clone)]
pub struct CachedBinaryFixture {
    pub agent_id: String,
    pub agent_version: String,
    pub cache_dir: PathBuf,
    pub executable_path: PathBuf,
}

/// Writes a cached binary distribution as an install would publish it, minus the
/// digests: it serves inventory flows (`list --installed`, uninstall), while
/// `run` and `serve` reuse needs archive- and payload-bound metadata that only
/// the real installer produces.
///
/// `platform` is the cache key (`darwin-aarch64`, `linux-x86_64`, ...) the entry
/// is published under, so a test can seed a platform other than the host's.
pub fn seed_cached_binary(
    harness: &Harness,
    agent_id: &str,
    agent_version: &str,
    platform: &str,
    script: &str,
) -> CachedBinaryFixture {
    const CMD: &str = "bin/agent";
    let cache_dir = harness
        .cache_root()
        .join("agents")
        .join(agent_id)
        .join(platform)
        .join(safe_path_component(agent_version));
    let executable_path = cache_dir.join("extracted").join(CMD);
    let executable_dir = executable_path.parent().expect("command path has a parent");
    fs::create_dir_all(executable_dir)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", executable_dir.display()));
    fs::write(&executable_path, script)
        .unwrap_or_else(|error| panic!("failed to write {}: {error}", executable_path.display()));
    fs::set_permissions(&executable_path, fs::Permissions::from_mode(0o755))
        .expect("failed to make the seeded executable runnable");

    let metadata = serde_json::json!({
        "agent_id": agent_id,
        "agent_version": agent_version,
        "platform": platform,
        "archive": format!("https://example.invalid/{agent_id}.tar.gz"),
        "cmd": CMD,
    });
    let metadata_path = cache_dir.join("metadata.json");
    fs::write(
        &metadata_path,
        serde_json::to_vec_pretty(&metadata).expect("cache metadata serializes"),
    )
    .unwrap_or_else(|error| panic!("failed to write {}: {error}", metadata_path.display()));

    CachedBinaryFixture {
        agent_id: agent_id.to_string(),
        agent_version: agent_version.to_string(),
        cache_dir,
        executable_path,
    }
}

/// Builds a `.tar.gz` laid out like a published binary distribution; `entry_path`
/// is what a registry target names in `cmd`.
pub fn binary_archive(entry_path: &str, executable: &str, extra_files: &[(&str, &str)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    append(&mut builder, entry_path, executable, 0o755);
    for (path, contents) in extra_files {
        append(&mut builder, path, contents, 0o644);
    }
    finish(builder)
}

/// Builds a `.tar.gz` whose last entry names `escape_path` verbatim, so a test
/// can hand an extractor a `../`-escaping entry that `tar::Header::set_path`
/// refuses to produce.
pub fn binary_archive_with_escaping_entry(
    entry_path: &str,
    executable: &str,
    escape_path: &str,
) -> Vec<u8> {
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    append(&mut builder, entry_path, executable, 0o755);

    let mut header = tar::Header::new_gnu();
    header.set_size(0);
    header.set_mode(0o644);
    let name = header.as_mut_bytes();
    let escape = escape_path.as_bytes();
    assert!(
        escape.len() <= name.len(),
        "tar names fit in the header's name field"
    );
    name[..escape.len()].copy_from_slice(escape);
    header.set_cksum();
    builder
        .append(&header, std::io::empty())
        .expect("failed to add the escaping entry to the fixture archive");

    finish(builder)
}

/// Builds a `.tar.gz` carrying the distribution plus `directories` directory
/// entries, enough entries to exceed the archive entry-count limit without also
/// exceeding the separate non-directory file limit.
///
/// Every entry names the same path, so extraction reaches the limit without the
/// cost of creating a directory per entry.
pub fn binary_archive_with_empty_directories(
    entry_path: &str,
    executable: &str,
    directories: usize,
) -> Vec<u8> {
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    append(&mut builder, entry_path, executable, 0o755);
    for _ in 0..directories {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        header.set_size(0);
        builder
            .append_data(&mut header, "lib/", std::io::empty())
            .expect("failed to add a directory entry to the fixture archive");
    }
    finish(builder)
}

/// Path, relative to the harness fixture directory, the mock catalog names as a
/// platform's binary archive.
pub fn catalog_archive_path(platform: &str) -> String {
    format!("binary/{platform}.tar.gz")
}

/// Writes `bytes` as the binary archive for every platform the mock catalog
/// declares, so the catalog's archive URLs and its `{archive_sha256}`
/// placeholder all agree with the served bytes.
pub fn write_catalog_archives(harness: &Harness, bytes: &[u8]) {
    for platform in platform_keys() {
        harness.write_fixture_bytes(&catalog_archive_path(platform), bytes);
    }
}

fn finish(builder: tar::Builder<GzEncoder<Vec<u8>>>) -> Vec<u8> {
    builder
        .into_inner()
        .expect("failed to finish the fixture archive")
        .finish()
        .expect("failed to finish the fixture archive compression")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes).as_slice())
}

fn append(builder: &mut tar::Builder<GzEncoder<Vec<u8>>>, path: &str, contents: &str, mode: u32) {
    let mut header = tar::Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(mode);
    header.set_cksum();
    builder
        .append_data(&mut header, path, contents.as_bytes())
        .unwrap_or_else(|error| panic!("failed to add {path} to the fixture archive: {error}"));
}

fn hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push_str(&format!("{byte:02x}"));
    }
    encoded
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Only plain ids and versions are supported, so the cache path needs no
/// sanitizing.
fn safe_path_component(value: &str) -> String {
    assert!(
        !value.is_empty()
            && !value.starts_with('.')
            && value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_')),
        "the harness only seeds cache paths for plain ids and versions: {value:?}"
    );
    value.to_string()
}

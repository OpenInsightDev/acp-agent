use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};

use super::{Harness, host_platform_key};

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
pub fn seed_cached_binary(
    harness: &Harness,
    agent_id: &str,
    agent_version: &str,
    script: &str,
) -> CachedBinaryFixture {
    const CMD: &str = "bin/agent";
    let cache_dir = harness
        .cache_root()
        .join("agents")
        .join(agent_id)
        .join(host_platform_key())
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
        "platform": host_platform_key(),
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

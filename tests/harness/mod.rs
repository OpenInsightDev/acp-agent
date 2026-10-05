//! Shared harness for the integration test files in `tests/`: the built
//! `acp-agent` binary, one hermetic environment per test, and shared fixtures.
//!
//! A test file pulls it in with `mod harness;`; a subdirectory module is not a
//! Cargo test target of its own.

#![allow(dead_code)] // each test target uses a different subset
#![allow(unused_imports)] // the re-exports serve whichever target needs them

mod agent;
mod cli;
mod daemon;
mod http;
mod registry;
mod server;

pub use agent::{AcpAgent, CachedBinaryFixture, binary_archive, seed_cached_binary, sha256_hex};
pub use cli::{CliOutput, Command};
pub use daemon::Daemon;
pub use http::{HttpResponse, free_port, http_get, http_post_json, wait_for_http_status};
pub use registry::MockCatalog;
pub use server::FixtureServer;

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;

const BINARY: &str = env!("CARGO_BIN_EXE_acp-agent");
const FALLBACK_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

pub const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(10);

/// One test's isolated filesystem and environment; two harnesses share no
/// directory, socket, or cache entry.
///
/// The socket gets its own `/tmp` directory because a Unix socket path is capped
/// at 104 bytes and a CI or sandbox `TMPDIR` is often nested deeper than that.
pub struct Harness {
    root: TempDir,
    socket_dir: TempDir,
    home: PathBuf,
    temp: PathBuf,
    bin: PathBuf,
    fixtures: PathBuf,
    socket: PathBuf,
    path: OsString,
}

impl Harness {
    pub fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("acp-e2e-")
            .tempdir()
            .expect("failed to create the harness root directory");
        let home = root.path().join("home");
        let temp = root.path().join("tmp");
        let bin = root.path().join("bin");
        let fixtures = root.path().join("fixtures");
        for directory in [&home, &temp, &bin, &fixtures] {
            fs::create_dir_all(directory).unwrap_or_else(|error| {
                panic!("failed to create {}: {error}", directory.display())
            });
        }

        let socket_dir = tempfile::Builder::new()
            .prefix("acp-e2e-")
            .tempdir_in("/tmp")
            .expect("failed to create the daemon socket directory in /tmp");
        // The daemon refuses a socket whose parent is not private.
        fs::set_permissions(socket_dir.path(), fs::Permissions::from_mode(0o700))
            .expect("failed to secure the daemon socket directory");
        let socket = socket_dir.path().join("daemon.sock");

        let path = match env::var_os("PATH") {
            Some(inherited) if !inherited.is_empty() => {
                let mut path = OsString::from(bin.as_os_str());
                path.push(":");
                path.push(inherited);
                path
            }
            _ => OsString::from(format!("{}:{FALLBACK_PATH}", bin.display())),
        };

        Self {
            root,
            socket_dir,
            home,
            temp,
            bin,
            fixtures,
            socket,
            path,
        }
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// What the *child* process resolves from `HOME`; [`dirs::cache_dir`] would
    /// answer for the test process instead. `XDG_CACHE_HOME` is deliberately
    /// unset, so `HOME` decides, and `installed_inventory` pins the result.
    pub fn cache_root(&self) -> PathBuf {
        #[cfg(target_os = "macos")]
        {
            self.home.join("Library/Caches").join("acp-agent")
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.home.join(".cache").join("acp-agent")
        }
    }

    pub fn temp_dir(&self) -> &Path {
        &self.temp
    }

    pub fn bin_dir(&self) -> &Path {
        &self.bin
    }

    pub fn fixtures_dir(&self) -> &Path {
        &self.fixtures
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// Fixture binaries come first so a test can shadow `npm`, `deno`, `uv`, or
    /// an agent executable.
    pub fn path_env(&self) -> &OsStr {
        &self.path
    }

    pub fn command(&self, args: &[&str]) -> Command {
        Command::new(self, args)
    }

    /// Runs `acp-agent` with `args` and captured output.
    pub async fn run(&self, args: &[&str]) -> CliOutput {
        self.command(args).output().await
    }

    pub fn write_fixture_file(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.fixtures.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("failed to create {}: {error}", parent.display()));
        }
        fs::write(&path, contents)
            .unwrap_or_else(|error| panic!("failed to write {}: {error}", path.display()));
        path
    }

    pub fn write_script(&self, name: &str, body: &str) -> PathBuf {
        assert!(
            !name.is_empty() && !name.contains('/'),
            "script name must be a bare file name: {name:?}"
        );
        let path = self.bin.join(name);
        fs::write(&path, body)
            .unwrap_or_else(|error| panic!("failed to write {}: {error}", path.display()));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap_or_else(|error| {
            panic!("failed to make {} executable: {error}", path.display())
        });
        path
    }

    /// Starts a foreground daemon bound to this harness's socket.
    pub async fn daemon(&self) -> Daemon {
        Daemon::start(self).await
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

/// Checked-in fixtures shared by every test file.
///
/// They live in the repository rather than in a harness temp directory so a
/// reviewer can read the exact catalog or archive a test asserts against.
pub fn repo_fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

pub fn host_platform_key() -> &'static str {
    match (env::consts::OS, env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-aarch64",
        ("macos", "x86_64") => "darwin-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("linux", "x86_64") => "linux-x86_64",
        (os, arch) => panic!("the harness does not support the {os}-{arch} platform"),
    }
}

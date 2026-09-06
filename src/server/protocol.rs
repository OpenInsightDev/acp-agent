//! The versioned Unix-domain control protocol used by the named-server daemon.
//!
//! This module is intentionally transport-only. The daemon and client layers own
//! command handling and rendering; this module owns the wire contract, endpoint
//! discovery, and bounded framing.

use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};

/// The control-protocol version, independent of the package version.
///
/// Wire-compatible changes must keep this value unchanged. A future incompatible
/// protocol can be introduced by incrementing it.
pub(crate) const PROTOCOL_VERSION: u32 = 2;

/// Environment variable used to override the control socket path.
pub(crate) const DAEMON_SOCKET_ENV: &str = "ACP_AGENT_DAEMON_SOCKET";

/// Maximum JSON payload accepted in one framed message.
pub(crate) const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// The largest path that can be represented by a Unix `sockaddr_un`.
#[cfg(target_os = "macos")]
const MAX_SOCKET_PATH_BYTES: usize = 104 - 1;
#[cfg(all(unix, not(target_os = "macos")))]
const MAX_SOCKET_PATH_BYTES: usize = 108 - 1;

/// Resolve the configured daemon socket and ensure its parent is private.
pub(crate) fn daemon_socket_path() -> Result<PathBuf> {
    socket_path_from(env::var_os(DAEMON_SOCKET_ENV))
}

/// Resolve a socket path from an explicit override or the deterministic default.
///
/// The explicit form is kept separate from [`daemon_socket_path`] so callers and
/// tests do not need to mutate the process environment. The returned path is
/// absolute, has a valid Unix-socket length, and has a parent directory with
/// mode `0700`.
pub(crate) fn socket_path_from(override_path: Option<OsString>) -> Result<PathBuf> {
    let path = match override_path {
        Some(path) => {
            if path.is_empty() {
                bail!("{DAEMON_SOCKET_ENV} must not be empty")
            }
            PathBuf::from(path)
        }
        None => default_socket_path()?,
    };

    validate_socket_path(&path)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .context("daemon socket path has no parent directory")?;
    ensure_private_directory(parent)?;
    Ok(path)
}

fn default_socket_path() -> Result<PathBuf> {
    let base = dirs::runtime_dir()
        .or_else(dirs::cache_dir)
        .or_else(dirs::home_dir)
        .context("failed to locate a user-scoped runtime directory")?;
    Ok(base.join("acp-agent").join("daemon").join("daemon.sock"))
}

fn validate_socket_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("daemon socket path must be absolute: {}", path.display())
    }
    if path.file_name().is_none() {
        bail!(
            "daemon socket path must name a socket file: {}",
            path.display()
        )
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if path.as_os_str().as_bytes().contains(&0) {
            bail!("daemon socket path must not contain NUL")
        }
        if path.as_os_str().as_bytes().len() > MAX_SOCKET_PATH_BYTES {
            bail!(
                "daemon socket path is too long (maximum is {MAX_SOCKET_PATH_BYTES} bytes): {}",
                path.display()
            )
        }
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    let created = match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                bail!(
                    "daemon socket parent is not a directory: {}",
                    path.display()
                )
            }
            false
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path).with_context(|| {
                format!(
                    "failed to create daemon socket directory {}",
                    path.display()
                )
            })?;
            true
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect daemon socket directory {}",
                    path.display()
                )
            });
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if created {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).with_context(
                || {
                    format!(
                        "failed to secure daemon socket directory {}",
                        path.display()
                    )
                },
            )?;
        }
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if mode != 0o700 {
            bail!(
                "daemon socket parent must have mode 0700 (found {mode:04o}): {}",
                path.display()
            )
        }
    }
    Ok(())
}

#[cfg(unix)]
/// Bind the daemon's Unix control socket without stale-endpoint recovery.
pub(crate) fn bind_daemon_socket() -> Result<(UnixListener, PathBuf)> {
    let path = daemon_socket_path()?;
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to bind daemon socket {}", path.display()))?;
    Ok((listener, path))
}

#[cfg(unix)]
#[expect(dead_code, reason = "used by the B4 Unix-socket client")]
/// Connect to the configured daemon Unix control socket.
pub(crate) async fn connect_daemon_socket() -> Result<UnixStream> {
    let path = daemon_socket_path()?;
    UnixStream::connect(&path)
        .await
        .with_context(|| format!("failed to connect to daemon socket {}", path.display()))
}

/// Read one bounded, four-byte big-endian length-prefixed JSON message.
pub(crate) async fn read_frame<R, T>(reader: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut header = [0; 4];
    reader
        .read_exact(&mut header)
        .await
        .context("failed to read protocol frame length")?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 {
        bail!("protocol frame has an empty payload")
    }
    if length > MAX_FRAME_SIZE {
        bail!("protocol frame payload exceeds {MAX_FRAME_SIZE} bytes")
    }

    let mut payload = vec![0; length];
    reader
        .read_exact(&mut payload)
        .await
        .context("truncated protocol frame payload")?;
    serde_json::from_slice(&payload).context("invalid protocol JSON payload")
}

/// Write one bounded, four-byte big-endian length-prefixed JSON message.
pub(crate) async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(value).context("failed to serialize protocol JSON payload")?;
    if payload.len() > MAX_FRAME_SIZE {
        bail!("protocol frame payload exceeds {MAX_FRAME_SIZE} bytes")
    }
    let length = u32::try_from(payload.len()).expect("MAX_FRAME_SIZE fits in a u32");
    writer
        .write_all(&length.to_be_bytes())
        .await
        .context("failed to write protocol frame length")?;
    writer
        .write_all(&payload)
        .await
        .context("failed to write protocol frame payload")?;
    writer
        .flush()
        .await
        .context("failed to flush protocol frame")?;
    Ok(())
}

/// One request is sent per Unix-stream connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RequestEnvelope {
    pub(crate) version: u32,
    #[serde(flatten)]
    pub(crate) command: Request,
}

/// Typed daemon commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", content = "payload", rename_all = "PascalCase")]
pub(crate) enum Request {
    Health,
    CreateInstance(CreateInstanceRequest),
    StopInstance(StopInstanceRequest),
    Register(RegisterRequest),
    Unregister(UnregisterRequest),
    List,
    Status(StatusRequest),
    Registrations(RegistrationsRequest),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CreateInstanceRequest {
    pub(crate) name: String,
    pub(crate) host: String,
    pub(crate) port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StopInstanceRequest {
    pub(crate) name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RegisterRequest {
    pub(crate) name: String,
    pub(crate) id: String,
    pub(crate) route: String,
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) cors_origins: Vec<String>,
    #[serde(default)]
    pub(crate) allow_any_origin: bool,
    #[serde(default = "default_true")]
    pub(crate) health_endpoint: bool,
    #[serde(default = "default_true")]
    pub(crate) readyz_endpoint: bool,
    pub(crate) max_processes: usize,
    #[serde(default)]
    pub(crate) yolo: bool,
    #[serde(default)]
    pub(crate) args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UnregisterRequest {
    pub(crate) name: String,
    pub(crate) id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StatusRequest {
    pub(crate) name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RegistrationsRequest {
    pub(crate) name: String,
}

fn default_true() -> bool {
    true
}

/// One typed response is sent for each request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum ResponseEnvelope {
    #[serde(rename = "ok")]
    Success {
        version: u32,
        result: Response,
    },
    Error {
        version: u32,
        error: ProtocolError,
    },
}

/// Typed results returned by daemon commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", content = "data", rename_all = "PascalCase")]
pub(crate) enum Response {
    Health(HealthResult),
    CreateInstance(InstanceResult),
    StopInstance(StopInstanceResult),
    Register(RegistrationResult),
    Unregister(UnregisterResult),
    List(ListResult),
    Status(InstanceResult),
    Registrations(RegistrationsResult),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HealthResult {
    pub(crate) protocol_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InstanceResult {
    pub(crate) name: String,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) address: String,
    pub(crate) state: InstanceState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StopInstanceResult {
    pub(crate) name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RegistrationResult {
    pub(crate) name: String,
    pub(crate) id: String,
    pub(crate) route: String,
    pub(crate) address: String,
    pub(crate) path: String,
    pub(crate) health_endpoint: bool,
    pub(crate) readyz_endpoint: bool,
    pub(crate) max_processes: usize,
    pub(crate) readiness: ReadinessResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReadinessResult {
    pub(crate) status: ReadinessStatus,
    pub(crate) attempts: u64,
    pub(crate) failures: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReadinessStatus {
    Disabled,
    Ready,
    NotReady,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UnregisterResult {
    pub(crate) name: String,
    pub(crate) id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ListResult {
    pub(crate) instances: Vec<InstanceResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RegistrationsResult {
    pub(crate) name: String,
    pub(crate) registrations: Vec<RegistrationResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InstanceState {
    Running,
    Stopping,
}

/// Stable error categories used by the control protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorCode {
    Unavailable,
    InvalidInput,
    NotFound,
    Conflict,
    Operation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProtocolError {
    pub(crate) code: ErrorCode,
    pub(crate) message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, path::Path};
    use tokio::io::duplex;

    fn sample_request() -> RequestEnvelope {
        RequestEnvelope {
            version: PROTOCOL_VERSION,
            command: Request::CreateInstance(CreateInstanceRequest {
                name: "work".into(),
                host: "127.0.0.1".into(),
                port: 0,
            }),
        }
    }

    #[tokio::test]
    async fn frame_roundtrip_preserves_typed_json() {
        let request = sample_request();
        let (mut writer, mut reader) = duplex(MAX_FRAME_SIZE + 16);
        write_frame(&mut writer, &request).await.unwrap();
        let decoded: RequestEnvelope = read_frame(&mut reader).await.unwrap();
        assert_eq!(decoded, request);
    }

    #[tokio::test]
    async fn frame_reader_rejects_oversized_payload() {
        let (mut writer, mut reader) = duplex(16);
        writer
            .write_all(&((MAX_FRAME_SIZE as u32) + 1).to_be_bytes())
            .await
            .unwrap();
        let error = read_frame::<_, serde_json::Value>(&mut reader)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeds"));
    }

    #[tokio::test]
    async fn frame_reader_rejects_truncated_payload() {
        let (mut writer, mut reader) = duplex(16);
        writer.write_all(&4_u32.to_be_bytes()).await.unwrap();
        writer.write_all(b"{}\n").await.unwrap();
        drop(writer);
        let error = read_frame::<_, serde_json::Value>(&mut reader)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("truncated"));
    }

    #[tokio::test]
    async fn frame_writer_rejects_oversized_payload() {
        let (mut writer, _reader) = duplex(MAX_FRAME_SIZE + 16);
        let error = write_frame(&mut writer, &"x".repeat(MAX_FRAME_SIZE + 1))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeds"));
    }

    #[test]
    fn socket_override_is_resolved_and_parent_is_private() {
        let directory = tempfile::Builder::new()
            .prefix("a")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = directory.path().join("nested").join("daemon.sock");
        let resolved = socket_path_from(Some(socket.clone().into_os_string())).unwrap();
        assert_eq!(resolved, socket);
        assert_eq!(
            std::fs::metadata(socket.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn socket_override_validation_rejects_relative_empty_and_too_long_paths() {
        let relative = socket_path_from(Some("daemon.sock".into()));
        assert!(relative.unwrap_err().to_string().contains("absolute"));

        let empty = socket_path_from(Some(OsString::new()));
        assert!(empty.unwrap_err().to_string().contains("empty"));

        let long_name = "x".repeat(MAX_SOCKET_PATH_BYTES + 1);
        let long_path = Path::new("/").join(long_name);
        let error = socket_path_from(Some(long_path.into_os_string())).unwrap_err();
        assert!(error.to_string().contains("too long"));
    }

    #[test]
    fn default_socket_path_is_deterministic_and_user_scoped() {
        let first = default_socket_path().unwrap();
        let second = default_socket_path().unwrap();
        assert_eq!(first, second);
        assert!(first.is_absolute());
        assert_eq!(
            first.file_name().and_then(|name| name.to_str()),
            Some("daemon.sock")
        );
    }

    #[test]
    fn protocol_serialization_is_stable() {
        let json = serde_json::to_string(&sample_request()).unwrap();
        assert_eq!(
            json,
            r#"{"version":2,"command":"CreateInstance","payload":{"name":"work","host":"127.0.0.1","port":0}}"#
        );
    }

    #[test]
    fn registration_results_roundtrip_with_address_and_readiness_statuses() {
        let response = ResponseEnvelope::Success {
            version: PROTOCOL_VERSION,
            result: Response::Registrations(RegistrationsResult {
                name: "work".into(),
                registrations: vec![
                    RegistrationResult {
                        name: "work".into(),
                        id: "disabled".into(),
                        route: "/disabled".into(),
                        address: "http://127.0.0.1:8010".into(),
                        path: "/acp".into(),
                        health_endpoint: true,
                        readyz_endpoint: false,
                        max_processes: 2,
                        readiness: ReadinessResult {
                            status: ReadinessStatus::Disabled,
                            attempts: 0,
                            failures: 0,
                            detail: None,
                        },
                    },
                    RegistrationResult {
                        name: "work".into(),
                        id: "ready".into(),
                        route: "/ready".into(),
                        address: "http://127.0.0.1:8010".into(),
                        path: "/acp".into(),
                        health_endpoint: true,
                        readyz_endpoint: true,
                        max_processes: 2,
                        readiness: ReadinessResult {
                            status: ReadinessStatus::Ready,
                            attempts: 1,
                            failures: 0,
                            detail: None,
                        },
                    },
                    RegistrationResult {
                        name: "work".into(),
                        id: "not-ready".into(),
                        route: "/not-ready".into(),
                        address: "http://127.0.0.1:8010".into(),
                        path: "/acp".into(),
                        health_endpoint: true,
                        readyz_endpoint: true,
                        max_processes: 2,
                        readiness: ReadinessResult {
                            status: ReadinessStatus::NotReady,
                            attempts: 2,
                            failures: 1,
                            detail: Some("spawn failed".into()),
                        },
                    },
                ],
            }),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains(r#""address":"http://127.0.0.1:8010""#));
        assert!(json.contains(r#""status":"disabled""#));
        assert!(json.contains(r#""status":"ready""#));
        assert!(json.contains(r#""status":"not_ready""#));
        assert!(json.contains(r#""detail":"spawn failed""#));

        let decoded: ResponseEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, response);
    }

    #[test]
    fn protocol_error_codes_are_stable() {
        let error = ProtocolError {
            code: ErrorCode::NotFound,
            message: "missing instance".into(),
        };
        assert_eq!(
            serde_json::to_string(&error).unwrap(),
            r#"{"code":"not_found","message":"missing instance"}"#
        );
    }
}

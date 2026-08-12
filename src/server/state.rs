use super::*;

pub(super) const SERVER_STATE_DIR_ENV: &str = "ACP_AGENT_SERVER_STATE_DIR";

#[cfg(test)]
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ServerFile {
    pub(super) name: String,
    pub(super) listen_host: String,
    pub(super) port: u16,
    pub(super) control_url: String,
    pub(super) pid: u32,
    pub(super) version: String,
}

#[derive(Debug, Clone)]
pub(super) struct ServerPaths {
    pub(super) directory: PathBuf,
}

impl ServerPaths {
    pub(super) fn discover() -> Result<Self> {
        if let Some(directory) = std::env::var_os(SERVER_STATE_DIR_ENV) {
            return Self::new(PathBuf::from(directory));
        }
        let base = dirs::cache_dir().context("failed to locate the platform cache directory")?;
        Self::new(base.join("acp-agent").join("servers"))
    }

    pub(super) fn new(directory: PathBuf) -> Result<Self> {
        ensure_private_directory(&directory)?;
        Ok(Self { directory })
    }

    pub(super) fn state_file(&self, name: &str) -> PathBuf {
        self.directory.join(format!("{name}.json"))
    }

    pub(super) fn log_file(&self, name: &str) -> PathBuf {
        self.directory.join(format!("{name}.log"))
    }
}

pub(super) fn ensure_private_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("failed to create server state directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).with_context(
            || format!("failed to secure server state directory {}", path.display()),
        )?;
    }
    // Windows inherits the current user's ACL from the platform cache root;
    // Unix modes do not have an ACL-equivalent meaning there.
    Ok(())
}

pub(super) fn open_private_file(path: &Path, append: bool) -> Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.create(true).write(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed to open private file {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to secure private file {}", path.display()))?;
    }
    // On Windows the newly created file inherits the private cache ACL.
    Ok(file)
}

pub(super) async fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {}", path.display()))
}

#[cfg(test)]
pub(super) fn write_private_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).context("failed to serialize server state")?;
    let parent = path
        .parent()
        .context("server state path has no parent directory")?;
    ensure_private_directory(parent)?;
    let file_name = path
        .file_name()
        .context("server state path has no file name")?
        .to_string_lossy();
    let (temporary, mut file) = loop {
        let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            counter
        ));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temporary) {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create {}", temporary.display()));
            }
        }
    };

    let result = (|| -> Result<()> {
        file.write_all(&bytes)
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", temporary.display()))?;
        drop(file);
        std::fs::rename(&temporary, path).with_context(|| {
            format!(
                "failed to replace server state {} with {}",
                path.display(),
                temporary.display()
            )
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

pub(super) fn write_private_json_exclusive(
    path: &Path,
    value: &impl Serialize,
    server_name: &str,
) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).context("failed to serialize server state")?;
    let parent = path
        .parent()
        .context("server state path has no parent directory")?;
    ensure_private_directory(parent)?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("server \"{server_name}\" is already starting or running")
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to create server state {}", path.display()));
        }
    };
    let result = (|| -> Result<()> {
        file.write_all(&bytes)
            .with_context(|| format!("failed to write {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

pub(super) fn control_url(listen_host: &str, port: u16) -> Result<String> {
    let host = match listen_host {
        "0.0.0.0" => "127.0.0.1",
        "::" | "[::]" => "::1",
        other => other,
    };
    http_url(host, port)
}

pub(super) fn public_url(host: &str, port: u16) -> Result<String> {
    http_url(host, port)
}

pub(super) fn http_url(host: &str, port: u16) -> Result<String> {
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let mut url = reqwest::Url::parse("http://localhost")?;
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        url.set_ip_host(address)
            .map_err(|_| anyhow::anyhow!("invalid server host {host:?}"))?;
    } else {
        url.set_host(Some(host))
            .map_err(|_| anyhow::anyhow!("invalid server host {host:?}"))?;
    }
    url.set_port(Some(port))
        .map_err(|_| anyhow::anyhow!("invalid server port {port}"))?;
    Ok(url.as_str().trim_end_matches('/').to_string())
}

pub(super) fn is_loopback_host(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

pub(super) async fn wait_for_shutdown(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow() {
        if receiver.changed().await.is_err() {
            break;
        }
    }
}

pub(super) async fn cleanup_failed_start(
    child: &mut tokio::process::Child,
    state_path: &Path,
    child_pid: u32,
) -> Result<()> {
    let _ = child.start_kill();
    child
        .wait()
        .await
        .context("failed to wait for server process during cleanup")?;
    let remove_state = match read_json::<ServerFile>(state_path).await {
        Ok(state) => state.pid == child_pid,
        Err(_) => true,
    };
    if !remove_state {
        return Ok(());
    }
    match tokio::fs::remove_file(state_path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to remove server state {}", state_path.display())),
    }
}

#[cfg(unix)]
pub(super) fn detach_process(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.as_std_mut().process_group(0);
}

#[cfg(windows)]
pub(super) fn detach_process(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command
        .as_std_mut()
        .creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
}

use super::daemon::{
    api_error_detail, load_live_server, server_is_alive, validate_name, validate_route,
};
use super::*;

/// Owns a detached server child until its state file proves that startup has
/// committed. Dropping the startup future must not leave an untracked daemon.
struct StartupChildGuard {
    child: Option<tokio::process::Child>,
    pid: u32,
    state_path: PathBuf,
}

impl StartupChildGuard {
    fn new(child: tokio::process::Child, pid: u32, state_path: PathBuf) -> Self {
        Self {
            child: Some(child),
            pid,
            state_path,
        }
    }

    fn child_mut(&mut self) -> &mut tokio::process::Child {
        self.child
            .as_mut()
            .expect("startup child guard was already disarmed")
    }

    fn disarm(mut self) -> tokio::process::Child {
        self.child
            .take()
            .expect("startup child guard was already disarmed")
    }
}

impl Drop for StartupChildGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let pid = self.pid;
        let state_path = self.state_path.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let cleanup = async move {
                let _ = cleanup_failed_start(&mut child, &state_path, pid).await;
            };
            handle.spawn(cleanup);
        } else {
            // A runtime may already be shutting down, so retain a synchronous
            // best-effort kill path when no executor can own the child.
            let _ = child.start_kill();
        }
    }
}

/// Starts a named ACP server in the background and returns its URL.
pub async fn start(options: StartOptions) -> Result<StartResult> {
    validate_name(&options.name)?;
    let executable = std::env::current_exe().context("failed to locate acp-agent executable")?;
    start_with(options, ServerPaths::discover()?, executable, START_TIMEOUT).await
}

pub(super) async fn start_with(
    options: StartOptions,
    paths: ServerPaths,
    executable: PathBuf,
    start_timeout: Duration,
) -> Result<StartResult> {
    validate_name(&options.name)?;
    let path = paths.state_file(&options.name);
    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        if let Ok(existing) = read_json::<ServerFile>(&path).await
            && server_is_alive(&existing).await
        {
            bail!(
                "server \"{}\" is already running at {}",
                options.name,
                public_url(&existing.listen_host, existing.port)?
            );
        }
        let _ = tokio::fs::remove_file(&path).await;
    }

    let log_path = paths.log_file(&options.name);
    let log = open_private_file(&log_path, true)?;
    let stderr = log
        .try_clone()
        .context("failed to clone server log handle")?;
    let mut command = Command::new(executable);
    command
        .arg("__server-run")
        .arg("--name")
        .arg(&options.name)
        .arg("--host")
        .arg(&options.host)
        .arg("--port")
        .arg(options.port.to_string())
        .env(SERVER_STATE_DIR_ENV, &paths.directory)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    detach_process(&mut command);
    let child = command.spawn().context("failed to start server process")?;
    let child_pid = child.id().context("failed to identify server process")?;
    let mut startup_child = StartupChildGuard::new(child, child_pid, path.clone());

    let ready = async {
        loop {
            if let Some(status) = startup_child
                .child_mut()
                .try_wait()
                .context("failed to inspect server process")?
            {
                bail!(
                    "server process exited with {status}; inspect {}",
                    log_path.display()
                );
            }
            if let Ok(state) = read_json::<ServerFile>(&path).await
                && state.name == options.name
                && state.version == SERVER_PROTOCOL_VERSION
                && state.pid == child_pid
                && server_is_alive(&state).await
            {
                return Ok(state);
            }
            sleep(Duration::from_millis(50)).await;
        }
    };
    let state = match timeout(start_timeout, ready).await {
        Ok(Ok(state)) => state,
        Ok(Err(error)) => {
            cleanup_failed_start(startup_child.child_mut(), &path, child_pid)
                .await
                .with_context(|| format!("failed to clean up after startup error: {error:#}"))?;
            drop(startup_child.disarm());
            return Err(error);
        }
        Err(_) => {
            cleanup_failed_start(startup_child.child_mut(), &path, child_pid)
                .await
                .context("failed to clean up after server startup timeout")?;
            drop(startup_child.disarm());
            bail!(
                "timed out waiting for server to start; inspect {}",
                log_path.display()
            );
        }
    };
    drop(startup_child.disarm());
    Ok(StartResult {
        name: state.name,
        address: public_url(&state.listen_host, state.port)?,
    })
}

/// Stops a named ACP server.
pub async fn stop(name: &str) -> Result<StopResult> {
    validate_name(name)?;
    let path = ServerPaths::discover()?.state_file(name);
    let state: ServerFile = read_json(&path)
        .await
        .with_context(|| format!("server \"{name}\" is not running"))?;
    if state.name != name || !server_is_alive(&state).await {
        bail!("server \"{name}\" is not running");
    }
    let response = reqwest::Client::new()
        .post(format!("{}/api/shutdown", state.control_url))
        .send()
        .await
        .with_context(|| format!("failed to contact server \"{name}\""))?;
    if !response.status().is_success() {
        bail!("server \"{name}\" rejected the shutdown request");
    }

    let wait = async {
        while tokio::fs::try_exists(&path).await.unwrap_or(false) {
            sleep(Duration::from_millis(50)).await;
        }
    };
    timeout(START_TIMEOUT, wait)
        .await
        .context("timed out waiting for server to stop")?;
    Ok(StopResult {
        name: name.to_string(),
    })
}

/// Registers an in-process agent router with a named ACP server.
pub async fn register(agent_id: &str, options: RegisterOptions) -> Result<RegisterResult> {
    validate_name(&options.name)?;
    let route = options.route.unwrap_or_else(|| format!("/{agent_id}"));
    validate_route(&route)?;
    let state = load_live_server(&options.name).await?;
    let registration = AgentRegistrationRequest {
        id: agent_id.to_string(),
        route: route.clone(),
        serve: AgentServeRequest {
            path: options.path,
            cors_origins: options.cors_origins,
            allow_any_origin: options.allow_any_origin,
            health_endpoint: options.health_endpoint,
            readyz_endpoint: options.readyz_endpoint,
            max_processes: options.max_processes,
            yolo: options.yolo,
            args: options.args,
        },
    };
    let response = reqwest::Client::new()
        .post(format!("{}/api/agents", state.control_url))
        .json(&registration)
        .send()
        .await
        .with_context(|| format!("failed to register with server \"{}\"", options.name))?;
    if !response.status().is_success() {
        let detail = response.text().await.unwrap_or_default();
        bail!(
            "server rejected agent registration: {}",
            api_error_detail(&detail)
        );
    }
    Ok(RegisterResult {
        agent_id: agent_id.to_string(),
        route,
        address: public_url(&state.listen_host, state.port)?,
    })
}

/// Removes an agent router from a named ACP server.
pub async fn unregister(agent_id: &str, name: &str) -> Result<UnregisterResult> {
    validate_name(name)?;
    let state = load_live_server(name).await?;
    let response = reqwest::Client::new()
        .delete(format!("{}/api/agents", state.control_url))
        .json(&AgentSelector {
            id: agent_id.to_string(),
        })
        .send()
        .await
        .with_context(|| format!("failed to contact server \"{name}\""))?;
    if !response.status().is_success() {
        let detail = response.text().await.unwrap_or_default();
        bail!(
            "server rejected agent unregistration: {}",
            api_error_detail(&detail)
        );
    }
    Ok(UnregisterResult {
        agent_id: agent_id.to_string(),
        server_name: name.to_string(),
    })
}

/// Lists named servers recorded in the state directory and their states.
///
/// Servers are sorted by name; each record reports the configured host, the
/// actual bound address, the daemon PID/version, and a lifecycle state.
pub async fn list() -> Result<Vec<ServerRecord>> {
    let paths = ServerPaths::discover()?;
    list_with_paths(&paths).await
}

pub(super) async fn list_with_paths(paths: &ServerPaths) -> Result<Vec<ServerRecord>> {
    let mut records = Vec::new();
    let mut entries = tokio::fs::read_dir(&paths.directory)
        .await
        .with_context(|| {
            format!(
                "failed to read server state directory {}",
                paths.directory.display()
            )
        })?;
    while let Some(entry) = entries.next_entry().await? {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        let Some(name) = strip_json_suffix(&file_name) else {
            continue;
        };
        let state = read_json::<ServerFile>(&paths.state_file(name))
            .await
            .with_context(|| format!("failed to inspect server state for {name:?}"))?;
        if state.name != name {
            bail!(
                "server state file {} contains server {:?}, expected {:?}",
                paths.state_file(name).display(),
                state.name,
                name
            );
        }
        records.push(server_record(name, &state).await);
    }
    records.sort_by(|left, right| left.name.cmp(&right.name));

    Ok(records)
}

/// Reports the state of a single named server.
///
/// Unlike `register`/`unregister`/`registrations`, a missing or stale record
/// is not an error: `status` is the diagnostic command, so `stopped`, `stale`
/// and `starting` are all reported as states (with exit code 0).
pub async fn status(name: &str) -> Result<ServerRecord> {
    validate_name(name)?;
    let paths = ServerPaths::discover()?;
    status_with_paths(&paths, name).await
}

pub(super) async fn status_with_paths(paths: &ServerPaths, name: &str) -> Result<ServerRecord> {
    validate_name(name)?;
    let state_path = paths.state_file(name);
    let exists = tokio::fs::try_exists(&state_path)
        .await
        .with_context(|| format!("failed to inspect server state for {name:?}"))?;
    let record = if !exists {
        ServerRecord {
            name: name.to_string(),
            state: ServerRunState::Stopped.as_str().to_string(),
            listen_host: None,
            port: None,
            address: None,
            pid: None,
            version: None,
        }
    } else {
        let state = read_json::<ServerFile>(&state_path)
            .await
            .with_context(|| format!("failed to inspect server state for {name:?}"))?;
        if state.name != name {
            bail!(
                "server state file {} contains server {:?}, expected {:?}",
                state_path.display(),
                state.name,
                name
            );
        }
        server_record(name, &state).await
    };
    Ok(record)
}

/// Lists the agent routes registered with a named server and probes each
/// route's readiness endpoint.
pub async fn registrations(name: &str) -> Result<Vec<RegistrationRecord>> {
    validate_name(name)?;
    let state = load_live_server(name).await?;
    let response = reqwest::Client::new()
        .get(format!("{}/api/registrations", state.control_url))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .with_context(|| format!("failed to contact server \"{name}\""))?;
    if !response.status().is_success() {
        bail!("server \"{name}\" rejected the registrations request");
    }
    let registrations: Vec<RegistrationInfo> = response
        .json()
        .await
        .context("failed to parse the registrations response")?;
    // Probe every route's readiness concurrently: a hung router must not
    // stall the other probes (each probe is bounded by its own timeout).
    let mut records = join_all(registrations.into_iter().map(|registration| {
        let state = &state;
        async move {
            let (readiness, detail) = probe_registration_readiness(state, &registration).await;
            RegistrationRecord {
                id: registration.id,
                route: registration.route,
                readiness,
                detail,
            }
        }
    }))
    .await;
    records.sort_by(|left, right| {
        left.route
            .cmp(&right.route)
            .then_with(|| left.id.cmp(&right.id))
    });

    Ok(records)
}

/// Tails a named server's log.
///
/// The tail is bounded by `lines`.
pub async fn logs(name: &str, lines: usize) -> Result<LogRecord> {
    validate_name(name)?;
    let log_path = ServerPaths::discover()?.log_file(name);
    let content = match read_log_tail(&log_path, lines).await {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "server \"{name}\" has no log file; start it with `acp-agent server start --name {name}`"
            );
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", log_path.display()));
        }
    };
    Ok(LogRecord {
        name: name.to_string(),
        lines: tail_lines(&content, lines)
            .into_iter()
            .map(str::to_string)
            .collect(),
    })
}

// Read backwards in bounded chunks so a short tail never loads an unbounded
// daemon log into memory.
pub(super) async fn read_log_tail(path: &Path, lines: usize) -> std::io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    if lines == 0 {
        return Ok(String::new());
    }
    let length = file.metadata().await?.len();
    if length == 0 {
        return Ok(String::new());
    }

    const CHUNK_SIZE: usize = 8192;
    let mut position = length;
    let mut chunks = Vec::new();
    let mut newline_count = 0usize;
    let mut bytes_read = 0usize;
    let required_newlines = lines.saturating_add(1);
    while position > 0 && newline_count < required_newlines {
        let remaining = MAX_LOG_TAIL_BYTES - bytes_read;
        if remaining == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "log tail exceeds the {} byte read limit before finding {lines} lines",
                    MAX_LOG_TAIL_BYTES
                ),
            ));
        }
        let amount = position.min(CHUNK_SIZE.min(remaining) as u64) as usize;
        position -= amount as u64;
        file.seek(std::io::SeekFrom::Start(position)).await?;
        let mut chunk = vec![0; amount];
        file.read_exact(&mut chunk).await?;
        newline_count += chunk.iter().filter(|byte| **byte == b'\n').count();
        bytes_read += amount;
        chunks.push(chunk);
    }
    chunks.reverse();
    let capacity = chunks.iter().map(Vec::len).sum();
    let mut bytes = Vec::with_capacity(capacity);
    for chunk in chunks {
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

pub(super) async fn server_record(name: &str, state: &ServerFile) -> ServerRecord {
    let run_state = observed_state(state).await;
    ServerRecord {
        name: name.to_string(),
        state: run_state.as_str().to_string(),
        listen_host: Some(state.listen_host.clone()),
        port: Some(state.port),
        address: control_url(&state.listen_host, state.port).ok(),
        pid: Some(state.pid),
        version: Some(state.version.clone()),
    }
}

pub(super) async fn observed_state(state: &ServerFile) -> ServerRunState {
    if server_is_alive(state).await {
        return ServerRunState::Running;
    }
    if process_alive(state.pid) {
        return ServerRunState::Starting;
    }
    ServerRunState::Stale
}

// Signal 0 probes existence without terminating the process; EPERM still
// means the process exists.
#[cfg(unix)]
pub(super) fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 never delivers a signal; `pid` comes from a state file
    // written by this tool, and negative values cannot occur for u32.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
pub(super) fn process_alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .map(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
        })
        .unwrap_or(false)
}

pub(super) async fn probe_registration_readiness(
    state: &ServerFile,
    registration: &RegistrationInfo,
) -> (String, Option<String>) {
    if !registration.readyz_endpoint {
        return ("disabled".to_string(), None);
    }
    let url = format!("{}{}/readyz", state.control_url, registration.route);
    let Ok(response) = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_millis(1500))
        .send()
        .await
    else {
        return (
            "unknown".to_string(),
            Some("readiness probe failed".to_string()),
        );
    };
    match response.status() {
        StatusCode::OK => ("ready".to_string(), None),
        StatusCode::SERVICE_UNAVAILABLE => {
            let detail = response.text().await.unwrap_or_default();
            let detail = detail.trim().to_string();
            ("not_ready".to_string(), Some(detail))
        }
        StatusCode::NOT_FOUND => ("disabled".to_string(), None),
        _ => (
            "unknown".to_string(),
            Some(format!("unexpected probe status {}", response.status())),
        ),
    }
}

pub(super) fn strip_json_suffix(name: &str) -> Option<&str> {
    name.strip_suffix(".json").filter(|name| !name.is_empty())
}

pub(super) fn tail_lines(content: &str, max_lines: usize) -> Vec<&str> {
    if max_lines == 0 {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let start = lines.len().saturating_sub(max_lines);
    lines[start..]
        .iter()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

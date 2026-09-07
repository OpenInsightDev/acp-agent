use super::*;
/// Appends one line per binary install attempt to `agent-install.log`.
///
/// Successes are logged too (cache hits included): a `ready` line is the only
/// way to confirm from outside the shell-less container which agent versions
/// are present in `/cache`; failures carry the full error chain. The blocking
/// file I/O runs on a blocking thread so a slow cache volume never stalls a
/// Tokio worker.
pub(crate) async fn record_install_log(
    agent: &RegistryAgent,
    platform: Platform,
    result: &Result<CachedBinary>,
) {
    let Ok(root_dir) = cache_root_dir() else {
        return;
    };
    record_install_log_in(&root_dir, agent, platform, result).await;
}

pub(crate) async fn record_install_log_in(
    root_dir: &Path,
    agent: &RegistryAgent,
    platform: Platform,
    result: &Result<CachedBinary>,
) {
    let platform = platform_cache_key(platform);
    let outcome = match result {
        Ok(cached) => format!(
            "ready agent={} version={} platform={} executable={}",
            agent.id,
            agent.version,
            platform,
            cached.executable_path.display()
        ),
        Err(error) => format!(
            "FAILED agent={} version={} platform={} error={error:#}",
            agent.id, agent.version, platform
        ),
    };
    let line = format!("[{}] {outcome}\n", utc_timestamp());
    let log_path = root_dir.join(INSTALL_LOG_FILE_NAME);
    let _ = tokio::task::spawn_blocking(move || append_install_log(&log_path, &line)).await;
}

pub(crate) fn append_install_log(path: &Path, line: &str) {
    if let Err(error) = append_install_log_inner(path, line) {
        eprintln!(
            "failed to append to agent install log {}: {error}",
            path.display()
        );
    }
}

pub(crate) fn append_install_log_inner(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Writers serialize on a dedicated lock file that is never renamed, so the
    // exclusive lock stays valid even though the atomic truncation below
    // replaces the log file with a new inode.
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(install_log_lock_path(path))?;
    lock_file.lock()?;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    if file.metadata()?.len() + line.len() as u64 > INSTALL_LOG_MAX_BYTES {
        drop(file);
        truncate_install_log(path)?;
        file = std::fs::OpenOptions::new().append(true).open(path)?;
    }
    file.write_all(line.as_bytes())
}

/// Dedicated cross-process lock file guarding [`append_install_log_inner`].
///
/// A sibling of the log file (never the log file itself) so that the lock
/// stays associated with one stable inode across atomic log rotation.
pub(crate) fn install_log_lock_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("install.log"))
        .to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

/// Keeps only the most recent tail so the append-only log stays bounded in a
/// long-lived cache volume; the leading partial line is dropped so retained
/// lines stay complete. The rewrite lands in a temporary sibling file that is
/// renamed into place while the caller holds the cross-process lock, so a
/// concurrent reader observes either the old or the new log, never a torn one.
pub(crate) fn truncate_install_log(path: &Path) -> std::io::Result<()> {
    let bytes = std::fs::read(path)?;
    let mut kept = bytes
        .iter()
        .copied()
        .skip(bytes.len().saturating_sub(INSTALL_LOG_TAIL_BYTES as usize))
        .collect::<Vec<_>>();
    // Drop the leading partial line so every retained line is complete.
    if let Some(newline) = kept.iter().position(|&byte| byte == b'\n') {
        kept.drain(..=newline);
    }
    let mut rewritten = format!(
        "[install log truncated; keeping the last {} bytes]\n",
        INSTALL_LOG_TAIL_BYTES
    )
    .into_bytes();
    rewritten.append(&mut kept);

    let temp_path = path.with_extension("log.tmp");
    std::fs::write(&temp_path, rewritten)?;
    std::fs::rename(&temp_path, path)
}

/// UTC timestamp in `YYYY-MM-DDTHH:MM:SSZ` form.
pub(crate) fn utc_timestamp() -> String {
    let now = OffsetDateTime::now_utc();
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z",
        year = now.year(),
        month = now.month() as u8,
        day = now.day(),
        hour = now.hour(),
        minute = now.minute(),
        second = now.second(),
    )
}

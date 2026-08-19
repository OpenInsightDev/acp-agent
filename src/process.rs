//! Cancellation-safe local child-process helpers.

use std::process::{ExitStatus, Output, Stdio};

use tokio::process::Command;

/// Configures a child so dropping its wait future terminates the complete
/// process tree owned by the command.
pub(crate) fn cancellable(command: &mut Command) -> &mut Command {
    command.kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    command
}

/// Configures a child for an internal wrapper process.  The wrapper must stay
/// in its supervisor's Unix process group so killing the supervisor also
/// reaches the wrapped executable.  Windows still uses a private Job Object.
pub(crate) fn cancellable_in_supervisor_group(command: &mut Command) -> &mut Command {
    command.kill_on_drop(true);
    command
}

/// Waits for a command while ensuring cancellation terminates its descendants.
pub(crate) async fn output(command: &mut Command) -> std::io::Result<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cancellable(command).spawn()?;
    let _guard = ProcessTreeGuard::new(&child, true)?;
    child.wait_with_output().await
}

/// Waits for a command with inherited streams while preserving cancellation.
pub(crate) async fn status(command: &mut Command) -> std::io::Result<ExitStatus> {
    status_with(command, false).await
}

/// Same as [`status`], but leaves Unix process-group membership to the caller.
/// This is used by the hidden executable wrapper launched by an ACP server.
pub(crate) async fn status_in_supervisor_group(
    command: &mut Command,
) -> std::io::Result<ExitStatus> {
    status_with(command, true).await
}

async fn status_with(
    command: &mut Command,
    in_supervisor_group: bool,
) -> std::io::Result<ExitStatus> {
    let mut child = if in_supervisor_group {
        cancellable_in_supervisor_group(command).spawn()?
    } else {
        cancellable(command).spawn()?
    };
    let _guard = ProcessTreeGuard::new(&child, !in_supervisor_group)?;
    child.wait().await
}

#[cfg(unix)]
struct ProcessTreeGuard {
    pid: Option<u32>,
}

#[cfg(windows)]
struct ProcessTreeGuard {
    job: Option<windows_sys::Win32::Foundation::HANDLE>,
}

#[cfg(not(any(unix, windows)))]
struct ProcessTreeGuard;

impl ProcessTreeGuard {
    fn new(child: &tokio::process::Child, owns_unix_process_group: bool) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                pid: owns_unix_process_group.then(|| child.id()).flatten(),
            })
        }
        #[cfg(windows)]
        {
            let _ = owns_unix_process_group;
            use std::os::windows::io::RawHandle;
            use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            };

            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() {
                return Err(std::io::Error::from_raw_os_error(unsafe {
                    GetLastError() as i32
                }));
            }
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            } != 0;
            let process: Option<RawHandle> = child.raw_handle();
            let assigned = configured
                && process.is_some_and(|handle| unsafe {
                    AssignProcessToJobObject(job, handle as HANDLE) != 0
                });
            if !assigned {
                unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self { job: Some(job) })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (child, owns_unix_process_group);
            Ok(Self)
        }
    }
}

impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // The process is the leader of a group created by `cancellable`.
            // A negative PID targets the group, including package-manager
            // descendants which would otherwise survive a dropped wait future.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        if let Some(job) = self.job.take() {
            // Closing a job configured with KILL_ON_JOB_CLOSE terminates every
            // process assigned to it, including descendants created later.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tempfile::tempdir;
    use tokio::sync::Notify;

    use super::*;

    #[tokio::test]
    async fn cancelled_wait_kills_the_process_group() {
        let temp = tempdir().unwrap();
        let child_pid = temp.path().join("child.pid");
        let started = Arc::new(Notify::new());
        let signal = Arc::clone(&started);
        let path = child_pid.display().to_string();
        let task = tokio::spawn(async move {
            let mut command = Command::new("sh");
            command.args(["-c", &format!("sleep 60 & echo $! > {path}; wait")]);
            signal.notify_one();
            status(&mut command).await
        });
        started.notified().await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !child_pid.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let pid: i32 = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(contents) = tokio::fs::read_to_string(&child_pid).await
                    && let Ok(pid) = contents.trim().parse()
                {
                    break pid;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.is_err());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let alive = unsafe { libc::kill(pid, 0) == 0 };
                if !alive {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("process-group descendant survived cancellation");
    }
}

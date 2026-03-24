use std::error::Error;
use std::fmt;
use std::io;
use std::process::{ExitStatus, Stdio};

use tokio::process::Command;

use crate::install::{
    CommandSpec, InstallError, PreparedCommand, apply_command_spec, binary_command_spec,
    download_archive, extract_archive, make_executable, package_command_spec, resolve_cmd_path,
};
use crate::registry::{
    AgentNotFoundError, FetchRegistryError, Platform, RegistryAgent, UnsupportedPlatformError,
    fetch_registry,
};

pub async fn run_agent(agent_id: &str, user_args: &[String]) -> Result<ExitStatus, RunError> {
    let registry = fetch_registry().await.map_err(RunError::FetchRegistry)?;
    let agent = registry
        .get_agent(agent_id)
        .map_err(RunError::AgentNotFound)?;

    let prepared = prepare_run_command(agent, user_args).await?;
    run_prepared_command(prepared, &agent.id).await
}

pub(crate) async fn prepare_run_command(
    agent: &RegistryAgent,
    user_args: &[String],
) -> Result<PreparedCommand, RunError> {
    if let Some(binary) = &agent.distribution.binary {
        let platform = Platform::current().map_err(RunError::UnsupportedPlatform)?;
        if let Some(target) = binary.for_platform(platform) {
            let temp_dir = tokio::task::spawn_blocking(tempfile::tempdir)
                .await
                .map_err(RunError::Join)?
                .map_err(RunError::Io)?;
            let archive_path = download_archive(target, temp_dir.path())
                .await
                .map_err(RunError::from_install)?;
            let extracted_dir = temp_dir.path().join("extracted");
            tokio::fs::create_dir_all(&extracted_dir)
                .await
                .map_err(RunError::Io)?;
            extract_archive(archive_path, extracted_dir.clone())
                .await
                .map_err(RunError::from_install)?;

            let executable_path = resolve_cmd_path(&extracted_dir, &target.cmd);
            let metadata = tokio::fs::metadata(&executable_path).await;
            if metadata
                .as_ref()
                .map(|metadata| !metadata.is_file())
                .unwrap_or(true)
            {
                return Err(RunError::BinaryNotFound {
                    archive: target.archive.clone(),
                    cmd: target.cmd.clone(),
                    resolved_path: executable_path,
                });
            }

            make_executable(&executable_path)
                .await
                .map_err(RunError::Io)?;

            return Ok(PreparedCommand {
                spec: binary_command_spec(executable_path, extracted_dir, target, user_args),
                temp_dir: Some(temp_dir),
            });
        }
    }

    if let Some(npx) = &agent.distribution.npx {
        return Ok(PreparedCommand {
            spec: package_command_spec(
                "npx",
                &npx.package,
                npx.args.as_ref(),
                npx.env.as_ref(),
                user_args,
            ),
            temp_dir: None,
        });
    }

    if let Some(uvx) = &agent.distribution.uvx {
        return Ok(PreparedCommand {
            spec: package_command_spec(
                "uvx",
                &uvx.package,
                uvx.args.as_ref(),
                uvx.env.as_ref(),
                user_args,
            ),
            temp_dir: None,
        });
    }

    Err(RunError::UnavailableDistribution {
        agent_id: agent.id.clone(),
    })
}

pub(crate) async fn run_prepared_command(
    prepared: PreparedCommand,
    subject: &str,
) -> Result<ExitStatus, RunError> {
    let _temp_dir = prepared.temp_dir;
    run_command_interactive(prepared.spec, subject).await
}

async fn run_command_interactive(spec: CommandSpec, subject: &str) -> Result<ExitStatus, RunError> {
    let program_display = spec.program.to_string_lossy().into_owned();
    let mut command = Command::new(&spec.program);
    apply_command_spec(&mut command, &spec);
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    command.status().await.map_err(|error| RunError::CommandIo {
        program: program_display,
        subject: subject.to_string(),
        source: error,
    })
}

#[derive(Debug)]
pub enum RunError {
    FetchRegistry(FetchRegistryError),
    AgentNotFound(AgentNotFoundError),
    UnsupportedPlatform(UnsupportedPlatformError),
    UnavailableDistribution {
        agent_id: String,
    },
    InvalidArchiveUrl(String),
    InvalidCommandPath(String),
    BinaryNotFound {
        archive: String,
        cmd: String,
        resolved_path: std::path::PathBuf,
    },
    UnsupportedArchiveFormat(String),
    UnsafeArchiveEntry(String),
    Request(reqwest::Error),
    Io(io::Error),
    Zip(zip::result::ZipError),
    Join(tokio::task::JoinError),
    CommandIo {
        program: String,
        subject: String,
        source: io::Error,
    },
}

impl RunError {
    pub(crate) fn from_install(error: InstallError) -> Self {
        match error {
            InstallError::InvalidArchiveUrl(url) => Self::InvalidArchiveUrl(url),
            InstallError::InvalidCommandPath(cmd) => Self::InvalidCommandPath(cmd),
            InstallError::BinaryNotFound {
                archive,
                cmd,
                resolved_path,
            } => Self::BinaryNotFound {
                archive,
                cmd,
                resolved_path,
            },
            InstallError::UnsupportedArchiveFormat(path) => Self::UnsupportedArchiveFormat(path),
            InstallError::UnsafeArchiveEntry(entry) => Self::UnsafeArchiveEntry(entry),
            InstallError::Request(error) => Self::Request(error),
            InstallError::Io(error) => Self::Io(error),
            InstallError::Zip(error) => Self::Zip(error),
            InstallError::Join(error) => Self::Join(error),
            InstallError::FetchRegistry(error) => Self::FetchRegistry(error),
            InstallError::AgentNotFound(error) => Self::AgentNotFound(error),
            InstallError::UnsupportedPlatform(error) => Self::UnsupportedPlatform(error),
            InstallError::UnavailableDistribution { agent_id } => {
                Self::UnavailableDistribution { agent_id }
            }
            InstallError::MissingHomeDirectory => Self::Io(io::Error::new(
                io::ErrorKind::NotFound,
                "Could not determine the current user's home directory",
            )),
            InstallError::CommandIo { program, source } => Self::CommandIo {
                program,
                subject: "command".to_string(),
                source,
            },
            InstallError::CommandFailed {
                program,
                subject,
                detail,
            } => Self::CommandIo {
                program,
                subject: format!("{subject}: {detail}"),
                source: io::Error::other("command failed"),
            },
        }
    }
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FetchRegistry(error) => write!(f, "{error}"),
            Self::AgentNotFound(error) => write!(f, "{error}"),
            Self::UnsupportedPlatform(error) => write!(f, "{error}"),
            Self::UnavailableDistribution { agent_id } => {
                write!(
                    f,
                    "Agent \"{agent_id}\" does not have a runnable distribution"
                )
            }
            Self::InvalidArchiveUrl(url) => write!(f, "Invalid archive URL: {url}"),
            Self::InvalidCommandPath(cmd) => write!(f, "Invalid binary command path: {cmd}"),
            Self::BinaryNotFound {
                archive,
                cmd,
                resolved_path,
            } => write!(
                f,
                "Downloaded {archive}, but could not find \"{cmd}\" at {}",
                resolved_path.display()
            ),
            Self::UnsupportedArchiveFormat(path) => {
                write!(f, "Unsupported archive format for {path}")
            }
            Self::UnsafeArchiveEntry(entry) => write!(f, "Archive contains unsafe path: {entry}"),
            Self::Request(error) => write!(f, "Network request failed: {error}"),
            Self::Io(error) => write!(f, "I/O failed: {error}"),
            Self::Zip(error) => write!(f, "ZIP extraction failed: {error}"),
            Self::Join(error) => write!(f, "Blocking task failed: {error}"),
            Self::CommandIo {
                program,
                subject,
                source,
            } => write!(f, "Failed to run {program} for {subject}: {source}"),
        }
    }
}

impl Error for RunError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Request(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Zip(error) => Some(error),
            Self::Join(error) => Some(error),
            Self::CommandIo { source, .. } => Some(source),
            Self::FetchRegistry(_)
            | Self::AgentNotFound(_)
            | Self::UnsupportedPlatform(_)
            | Self::UnavailableDistribution { .. }
            | Self::InvalidArchiveUrl(_)
            | Self::InvalidCommandPath(_)
            | Self::BinaryNotFound { .. }
            | Self::UnsupportedArchiveFormat(_)
            | Self::UnsafeArchiveEntry(_) => None,
        }
    }
}

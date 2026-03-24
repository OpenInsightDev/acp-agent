use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};

use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use tokio::fs;
use tokio::process::Command;
use tokio::task;
use zip::ZipArchive;

use crate::registry::{
    AgentNotFoundError, BinaryTarget, FetchRegistryError, NpxDistribution, Platform, Registry,
    RegistryAgent, UnsupportedPlatformError, UvxDistribution, fetch_registry,
};

pub async fn install_agent(agent_id: &str) -> Result<InstallOutcome, InstallError> {
    let registry = fetch_registry()
        .await
        .map_err(InstallError::FetchRegistry)?;
    let agent = registry
        .get_agent(agent_id)
        .map_err(InstallError::AgentNotFound)?;

    install_from_registry(&registry, agent).await
}

pub async fn install_from_registry(
    _registry: &Registry,
    agent: &RegistryAgent,
) -> Result<InstallOutcome, InstallError> {
    if let Some(binary) = &agent.distribution.binary {
        let platform = Platform::current().map_err(InstallError::UnsupportedPlatform)?;
        if let Some(target) = binary.for_platform(platform) {
            return install_binary(agent, target).await;
        }
    }

    if let Some(npx) = &agent.distribution.npx {
        return install_npx(agent, npx).await;
    }

    if let Some(uvx) = &agent.distribution.uvx {
        return install_uvx(agent, uvx).await;
    }

    Err(InstallError::UnavailableDistribution {
        agent_id: agent.id.clone(),
    })
}

async fn install_npx(
    agent: &RegistryAgent,
    distribution: &NpxDistribution,
) -> Result<InstallOutcome, InstallError> {
    run_command(
        "npm",
        ["i", "-g", distribution.package.as_str()],
        &format!("npm package {}", distribution.package),
    )
    .await?;

    Ok(InstallOutcome::PackageManager {
        agent_id: agent.id.clone(),
        method: InstallMethod::Npx,
        package: distribution.package.clone(),
    })
}

async fn install_uvx(
    agent: &RegistryAgent,
    distribution: &UvxDistribution,
) -> Result<InstallOutcome, InstallError> {
    run_command(
        "uv",
        ["tool", "install", distribution.package.as_str()],
        &format!("uv package {}", distribution.package),
    )
    .await?;

    Ok(InstallOutcome::PackageManager {
        agent_id: agent.id.clone(),
        method: InstallMethod::Uvx,
        package: distribution.package.clone(),
    })
}

async fn install_binary(
    agent: &RegistryAgent,
    target: &BinaryTarget,
) -> Result<InstallOutcome, InstallError> {
    let temp_dir = task::spawn_blocking(tempfile::tempdir)
        .await
        .map_err(InstallError::Join)?
        .map_err(InstallError::Io)?;
    let archive_path = download_archive(target, temp_dir.path()).await?;
    let extracted_dir = temp_dir.path().join("extracted");
    fs::create_dir_all(&extracted_dir)
        .await
        .map_err(InstallError::Io)?;
    extract_archive(archive_path, extracted_dir.clone()).await?;

    let source_path = resolve_cmd_path(&extracted_dir, &target.cmd);
    let source_metadata = fs::metadata(&source_path).await;
    if source_metadata
        .as_ref()
        .map(|metadata| !metadata.is_file())
        .unwrap_or(true)
    {
        return Err(InstallError::BinaryNotFound {
            archive: target.archive.clone(),
            cmd: target.cmd.clone(),
            resolved_path: source_path,
        });
    }

    let install_dir = user_install_dir()?;
    fs::create_dir_all(&install_dir)
        .await
        .map_err(InstallError::Io)?;
    let file_name = source_path
        .file_name()
        .ok_or_else(|| InstallError::InvalidCommandPath(target.cmd.clone()))?;
    let destination = install_dir.join(file_name);
    fs::copy(&source_path, &destination)
        .await
        .map_err(InstallError::Io)?;
    make_executable(&destination)
        .await
        .map_err(InstallError::Io)?;

    Ok(InstallOutcome::Binary {
        agent_id: agent.id.clone(),
        path: destination,
    })
}

pub(crate) async fn download_archive(
    target: &BinaryTarget,
    temp_dir: &Path,
) -> Result<PathBuf, InstallError> {
    let url = reqwest::Url::parse(&target.archive)
        .map_err(|_| InstallError::InvalidArchiveUrl(target.archive.clone()))?;
    let archive_name = url
        .path_segments()
        .and_then(|segments| segments.last())
        .filter(|segment| !segment.is_empty())
        .unwrap_or("download.bin");
    let destination = temp_dir.join(archive_name);

    let response = reqwest::get(url).await.map_err(InstallError::Request)?;
    let response = response.error_for_status().map_err(InstallError::Request)?;
    let bytes = response.bytes().await.map_err(InstallError::Request)?;
    fs::write(&destination, bytes.as_ref())
        .await
        .map_err(InstallError::Io)?;
    Ok(destination)
}

pub(crate) async fn extract_archive(
    archive_path: PathBuf,
    destination: PathBuf,
) -> Result<(), InstallError> {
    task::spawn_blocking(move || extract_archive_blocking(&archive_path, &destination))
        .await
        .map_err(InstallError::Join)?
}

fn extract_archive_blocking(archive_path: &Path, destination: &Path) -> Result<(), InstallError> {
    let file_name = archive_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if file_name.ends_with(".zip") {
        return extract_zip(archive_path, destination);
    }

    if file_name.ends_with(".tar.gz") || file_name.ends_with(".tgz") {
        let file = File::open(archive_path).map_err(InstallError::Io)?;
        let decoder = GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        archive.unpack(destination).map_err(InstallError::Io)?;
        return Ok(());
    }

    if file_name.ends_with(".tar.bz2") || file_name.ends_with(".tbz2") {
        let file = File::open(archive_path).map_err(InstallError::Io)?;
        let decoder = BzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        archive.unpack(destination).map_err(InstallError::Io)?;
        return Ok(());
    }

    let fallback_path = destination.join(archive_path.file_name().ok_or_else(|| {
        InstallError::UnsupportedArchiveFormat(archive_path.display().to_string())
    })?);
    std::fs::copy(archive_path, fallback_path).map_err(InstallError::Io)?;
    Ok(())
}

fn extract_zip(archive_path: &Path, destination: &Path) -> Result<(), InstallError> {
    let file = File::open(archive_path).map_err(InstallError::Io)?;
    let mut archive = ZipArchive::new(file).map_err(InstallError::Zip)?;

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(InstallError::Zip)?;
        let enclosed = entry
            .enclosed_name()
            .map(PathBuf::from)
            .ok_or_else(|| InstallError::UnsafeArchiveEntry(entry.name().to_string()))?;
        let output_path = destination.join(enclosed);

        if entry.name().ends_with('/') {
            std::fs::create_dir_all(&output_path).map_err(InstallError::Io)?;
            continue;
        }

        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent).map_err(InstallError::Io)?;
        }

        let mut output = File::create(&output_path).map_err(InstallError::Io)?;
        io::copy(&mut entry, &mut output).map_err(InstallError::Io)?;
    }

    Ok(())
}

pub(crate) fn resolve_cmd_path(extracted_dir: &Path, cmd: &str) -> PathBuf {
    let sanitized = cmd.trim_start_matches("./");
    let candidate = PathBuf::from(sanitized);
    if candidate.is_absolute() {
        return candidate;
    }

    let mut resolved = extracted_dir.to_path_buf();
    for component in candidate.components() {
        match component {
            Component::CurDir => {}
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}

pub(crate) fn package_command_spec(
    program: &str,
    package: &str,
    args: Option<&Vec<String>>,
    env: Option<&crate::registry::Environment>,
    user_args: &[String],
) -> CommandSpec {
    let mut command_args = vec![OsString::from(package)];
    if let Some(args) = args {
        command_args.extend(args.iter().cloned().map(OsString::from));
    }
    command_args.extend(user_args.iter().cloned().map(OsString::from));

    CommandSpec {
        program: OsString::from(program),
        args: command_args,
        env: clone_env_pairs(env),
        current_dir: None,
    }
}

pub(crate) fn binary_command_spec(
    executable_path: PathBuf,
    extracted_dir: PathBuf,
    target: &BinaryTarget,
    user_args: &[String],
) -> CommandSpec {
    let mut args: Vec<OsString> = target
        .args
        .as_ref()
        .into_iter()
        .flatten()
        .cloned()
        .map(OsString::from)
        .collect();
    args.extend(user_args.iter().cloned().map(OsString::from));

    CommandSpec {
        program: executable_path.into_os_string(),
        args,
        env: clone_env_pairs(target.env.as_ref()),
        current_dir: Some(extracted_dir),
    }
}

fn clone_env_pairs(env: Option<&crate::registry::Environment>) -> Vec<(OsString, OsString)> {
    env.into_iter()
        .flat_map(|pairs| pairs.iter())
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect()
}

fn user_install_dir() -> Result<PathBuf, InstallError> {
    #[cfg(windows)]
    {
        let home = dirs::home_dir().ok_or(InstallError::MissingHomeDirectory)?;
        Ok(home.join(".acp-agent").join("bin"))
    }

    #[cfg(not(windows))]
    {
        let home = dirs::home_dir().ok_or(InstallError::MissingHomeDirectory)?;
        Ok(home.join(".local").join("bin"))
    }
}

pub(crate) async fn make_executable(path: &Path) -> Result<(), io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path).await?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).await?;
    }

    #[cfg(not(unix))]
    {
        let _ = path;
    }

    Ok(())
}

async fn run_command<I, S>(program: &str, args: I, subject: &str) -> Result<(), InstallError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let args_vec: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let output = Command::new(program)
        .args(&args_vec)
        .output()
        .await
        .map_err(|error| InstallError::CommandIo {
            program: program.to_string(),
            source: error,
        })?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() { stderr } else { stdout };

    Err(InstallError::CommandFailed {
        program: program.to_string(),
        subject: subject.to_string(),
        detail,
    })
}

pub(crate) fn apply_command_spec(command: &mut Command, spec: &CommandSpec) {
    command.args(&spec.args);

    if let Some(current_dir) = &spec.current_dir {
        command.current_dir(current_dir);
    }

    for (key, value) in &spec.env {
        command.env(key, value);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandSpec {
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
    pub(crate) env: Vec<(OsString, OsString)>,
    pub(crate) current_dir: Option<PathBuf>,
}

#[derive(Debug)]
pub(crate) struct PreparedCommand {
    pub(crate) spec: CommandSpec,
    pub(crate) temp_dir: Option<tempfile::TempDir>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethod {
    Binary,
    Npx,
    Uvx,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    Binary {
        agent_id: String,
        path: PathBuf,
    },
    PackageManager {
        agent_id: String,
        method: InstallMethod,
        package: String,
    },
}

impl fmt::Display for InstallOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Binary { agent_id, path } => {
                write!(f, "Installed {agent_id} to {}", path.display())
            }
            Self::PackageManager {
                agent_id,
                method,
                package,
            } => {
                let installer = match method {
                    InstallMethod::Binary => "binary",
                    InstallMethod::Npx => "npm",
                    InstallMethod::Uvx => "uv",
                };
                write!(f, "Installed {agent_id} via {installer}: {package}")
            }
        }
    }
}

#[derive(Debug)]
pub enum InstallError {
    FetchRegistry(FetchRegistryError),
    AgentNotFound(AgentNotFoundError),
    UnsupportedPlatform(UnsupportedPlatformError),
    UnavailableDistribution {
        agent_id: String,
    },
    InvalidArchiveUrl(String),
    UnsupportedArchiveFormat(String),
    InvalidCommandPath(String),
    BinaryNotFound {
        archive: String,
        cmd: String,
        resolved_path: PathBuf,
    },
    UnsafeArchiveEntry(String),
    MissingHomeDirectory,
    Request(reqwest::Error),
    Io(io::Error),
    Zip(zip::result::ZipError),
    Join(tokio::task::JoinError),
    CommandIo {
        program: String,
        source: io::Error,
    },
    CommandFailed {
        program: String,
        subject: String,
        detail: String,
    },
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FetchRegistry(error) => write!(f, "{error}"),
            Self::AgentNotFound(error) => write!(f, "{error}"),
            Self::UnsupportedPlatform(error) => write!(f, "{error}"),
            Self::UnavailableDistribution { agent_id } => {
                write!(
                    f,
                    "Agent \"{agent_id}\" does not have an installable distribution"
                )
            }
            Self::InvalidArchiveUrl(url) => write!(f, "Invalid archive URL: {url}"),
            Self::UnsupportedArchiveFormat(path) => {
                write!(f, "Unsupported archive format for {path}")
            }
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
            Self::UnsafeArchiveEntry(entry) => write!(f, "Archive contains unsafe path: {entry}"),
            Self::MissingHomeDirectory => {
                write!(f, "Could not determine the current user's home directory")
            }
            Self::Request(error) => write!(f, "Network request failed: {error}"),
            Self::Io(error) => write!(f, "I/O failed: {error}"),
            Self::Zip(error) => write!(f, "ZIP extraction failed: {error}"),
            Self::Join(error) => write!(f, "Blocking task failed: {error}"),
            Self::CommandIo { program, source } => {
                write!(f, "Failed to execute {program}: {source}")
            }
            Self::CommandFailed {
                program,
                subject,
                detail,
            } => {
                if detail.is_empty() {
                    write!(f, "{program} failed while installing {subject}")
                } else {
                    write!(f, "{program} failed while installing {subject}: {detail}")
                }
            }
        }
    }
}

impl Error for InstallError {
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
            | Self::UnsupportedArchiveFormat(_)
            | Self::InvalidCommandPath(_)
            | Self::BinaryNotFound { .. }
            | Self::UnsafeArchiveEntry(_)
            | Self::MissingHomeDirectory
            | Self::CommandFailed { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::AgentDistribution;

    fn sample_agent() -> RegistryAgent {
        RegistryAgent {
            id: "demo".to_string(),
            name: "Demo".to_string(),
            version: "1.0.0".to_string(),
            description: "Demo agent".to_string(),
            repository: None,
            website: None,
            authors: vec!["ACP".to_string()],
            license: "MIT".to_string(),
            icon: None,
            distribution: AgentDistribution {
                binary: None,
                npx: None,
                uvx: None,
            },
        }
    }

    #[test]
    fn resolves_relative_cmd_paths() {
        let base = Path::new("/tmp/acp-agent");
        let resolved = resolve_cmd_path(base, "./dist-package/cursor-agent");
        assert_eq!(resolved, base.join("dist-package").join("cursor-agent"));
    }

    #[tokio::test]
    async fn reports_missing_distribution() {
        let agent = sample_agent();

        let error = install_from_registry(
            &Registry {
                version: "1".to_string(),
                agents: vec![],
                extensions: None,
            },
            &agent,
        )
        .await
        .expect_err("install should fail");

        match error {
            InstallError::UnavailableDistribution { agent_id } => assert_eq!(agent_id, "demo"),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn builds_npx_command_spec_with_args_and_env() {
        let mut env = crate::registry::Environment::new();
        env.insert("FOO".to_string(), "bar".to_string());

        let spec = package_command_spec(
            "npx",
            "@example/demo",
            Some(&vec!["--acp".to_string()]),
            Some(&env),
            &["--model".to_string(), "gpt-5".to_string()],
        );

        assert_eq!(spec.program, OsString::from("npx"));
        assert_eq!(
            spec.args,
            vec![
                OsString::from("@example/demo"),
                OsString::from("--acp"),
                OsString::from("--model"),
                OsString::from("gpt-5"),
            ]
        );
        assert_eq!(
            spec.env,
            vec![(OsString::from("FOO"), OsString::from("bar"))]
        );
        assert!(spec.current_dir.is_none());
    }

    #[test]
    fn builds_binary_command_spec_with_current_dir() {
        let executable = PathBuf::from("/tmp/acp-agent/bin/demo");
        let root = PathBuf::from("/tmp/acp-agent");
        let target = BinaryTarget {
            archive: "https://example.com/demo.tar.gz".to_string(),
            cmd: "./bin/demo".to_string(),
            args: Some(vec!["serve".to_string()]),
            env: None,
        };

        let spec = binary_command_spec(
            executable.clone(),
            root.clone(),
            &target,
            &["--port".to_string(), "3000".to_string()],
        );

        assert_eq!(spec.program, executable.into_os_string());
        assert_eq!(
            spec.args,
            vec![
                OsString::from("serve"),
                OsString::from("--port"),
                OsString::from("3000"),
            ]
        );
        assert_eq!(spec.current_dir, Some(root));
    }
}

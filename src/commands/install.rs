use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use tokio::fs;
use tokio::process::Command;
use tokio::task;
use zip::ZipArchive;

use crate::registry::{
    BinaryTarget, NpxDistribution, Platform, Registry, RegistryAgent, UvxDistribution,
    fetch_registry,
};

/// Installs an agent by ID using the configured registry distribution.
///
/// The function mirrors the CLI `install` subcommand and returns a
/// descriptive `InstallOutcome` so callers can log where the agent ended up.
pub async fn install_agent(agent_id: &str) -> Result<InstallOutcome> {
    let registry = fetch_registry().await?;
    let agent = registry.get_agent(agent_id)?;

    install_from_registry(&registry, agent).await
}

/// Core installer that inspects each distribution in priority order.
///
/// Binary archives are installed to the user-local bin directory when a
/// platform-matching release exists; otherwise the function falls back to npm or
/// uv package installers depending on what the registry exposes.
pub async fn install_from_registry(
    _registry: &Registry,
    agent: &RegistryAgent,
) -> Result<InstallOutcome> {
    if let Some(binary) = &agent.distribution.binary {
        let platform = Platform::current()?;
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

    Err(anyhow!(
        "agent \"{}\" does not have an installable distribution",
        agent.id
    ))
}

async fn install_npx(
    agent: &RegistryAgent,
    distribution: &NpxDistribution,
) -> Result<InstallOutcome> {
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
) -> Result<InstallOutcome> {
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

async fn install_binary(agent: &RegistryAgent, target: &BinaryTarget) -> Result<InstallOutcome> {
    let temp_dir = task::spawn_blocking(tempfile::tempdir)
        .await
        .context("blocking task failed while creating temporary directory")?
        .context("failed to create temporary directory")?;
    let archive_path = download_archive(target, temp_dir.path()).await?;
    let extracted_dir = temp_dir.path().join("extracted");
    fs::create_dir_all(&extracted_dir)
        .await
        .with_context(|| format!("failed to create {}", extracted_dir.display()))?;
    extract_archive(archive_path, extracted_dir.clone()).await?;

    let source_path = resolve_cmd_path(&extracted_dir, &target.cmd);
    let source_metadata = fs::metadata(&source_path).await;
    if source_metadata
        .as_ref()
        .map(|metadata| !metadata.is_file())
        .unwrap_or(true)
    {
        bail!(
            "downloaded {}, but could not find \"{}\" at {}",
            target.archive,
            target.cmd,
            source_path.display()
        );
    }

    let install_dir = user_install_dir()?;
    fs::create_dir_all(&install_dir)
        .await
        .with_context(|| format!("failed to create {}", install_dir.display()))?;
    let file_name = source_path
        .file_name()
        .ok_or_else(|| anyhow!("invalid binary command path: {}", target.cmd))?;
    let destination = install_dir.join(file_name);
    fs::copy(&source_path, &destination)
        .await
        .with_context(|| {
            format!(
                "failed to copy {} to {}",
                source_path.display(),
                destination.display()
            )
        })?;
    make_executable(&destination).await?;

    Ok(InstallOutcome::Binary {
        agent_id: agent.id.clone(),
        path: destination,
    })
}

pub(crate) async fn download_archive(target: &BinaryTarget, temp_dir: &Path) -> Result<PathBuf> {
    let url = reqwest::Url::parse(&target.archive)
        .with_context(|| format!("invalid archive URL: {}", target.archive))?;
    let archive_name = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|segment| !segment.is_empty())
        .unwrap_or("download.bin");
    let destination = temp_dir.join(archive_name);

    let response = reqwest::get(url)
        .await
        .with_context(|| format!("failed to download archive from {}", target.archive))?;
    let response = response
        .error_for_status()
        .with_context(|| format!("failed to download archive from {}", target.archive))?;
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("failed to read archive response from {}", target.archive))?;
    fs::write(&destination, bytes.as_ref())
        .await
        .with_context(|| {
            format!(
                "failed to write downloaded archive to {}",
                destination.display()
            )
        })?;
    Ok(destination)
}

pub(crate) async fn extract_archive(archive_path: PathBuf, destination: PathBuf) -> Result<()> {
    task::spawn_blocking(move || extract_archive_blocking(&archive_path, &destination))
        .await
        .context("blocking task failed while extracting archive")?
}

fn extract_archive_blocking(archive_path: &Path, destination: &Path) -> Result<()> {
    let file_name = archive_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if file_name.ends_with(".zip") {
        return extract_zip(archive_path, destination);
    }

    if file_name.ends_with(".tar.gz") || file_name.ends_with(".tgz") {
        let file = File::open(archive_path)
            .with_context(|| format!("failed to open archive {}", archive_path.display()))?;
        let decoder = GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        archive
            .unpack(destination)
            .with_context(|| format!("failed to unpack archive into {}", destination.display()))?;
        return Ok(());
    }

    if file_name.ends_with(".tar.bz2") || file_name.ends_with(".tbz2") {
        let file = File::open(archive_path)
            .with_context(|| format!("failed to open archive {}", archive_path.display()))?;
        let decoder = BzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        archive
            .unpack(destination)
            .with_context(|| format!("failed to unpack archive into {}", destination.display()))?;
        return Ok(());
    }

    let file_name = archive_path
        .file_name()
        .ok_or_else(|| anyhow!("unsupported archive format for {}", archive_path.display()))?;
    let fallback_path = destination.join(file_name);
    std::fs::copy(archive_path, &fallback_path).with_context(|| {
        format!(
            "failed to copy archive {} to {}",
            archive_path.display(),
            fallback_path.display()
        )
    })?;
    Ok(())
}

fn extract_zip(archive_path: &Path, destination: &Path) -> Result<()> {
    let file = File::open(archive_path)
        .with_context(|| format!("failed to open archive {}", archive_path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("failed to read ZIP archive {}", archive_path.display()))?;

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("failed to read ZIP entry {index}"))?;
        let enclosed = entry
            .enclosed_name()
            .ok_or_else(|| anyhow!("archive contains unsafe path: {}", entry.name()))?;
        let output_path = destination.join(enclosed);

        if entry.name().ends_with('/') {
            std::fs::create_dir_all(&output_path)
                .with_context(|| format!("failed to create {}", output_path.display()))?;
            continue;
        }

        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let mut output = File::create(&output_path)
            .with_context(|| format!("failed to create {}", output_path.display()))?;
        io::copy(&mut entry, &mut output)
            .with_context(|| format!("failed to extract {}", output_path.display()))?;
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

fn user_install_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow!("could not determine the current user's home directory"))?;
        Ok(home.join(".acp-agent").join("bin"))
    }

    #[cfg(not(windows))]
    {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow!("could not determine the current user's home directory"))?;
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

async fn run_command<I, S>(program: &str, args: I, subject: &str) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let args_vec: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let output = Command::new(program)
        .args(&args_vec)
        .output()
        .await
        .with_context(|| format!("failed to run {program}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() { stderr } else { stdout };

    if detail.is_empty() {
        bail!("failed to run {program} for {subject}");
    }

    bail!("failed to run {program} for {subject}: {detail}");
}

/// Identifier for how an agent was installed when the CLI reports success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethod {
    /// The registry provided a ready-to-run binary archive.
    Binary,
    /// The registry points to an npm package invoking `npx`.
    Npx,
    /// The registry points to a uvx package invoking `uv`.
    Uvx,
}

/// Outcome data that is printed by the `install` subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// A binary archive was downloaded and copied under the user bin directory.
    Binary {
        /// ID of the agent that was installed.
        agent_id: String,
        /// Filesystem path of the copied executable.
        path: PathBuf,
    },
    /// A package manager (npm or uv) installed a wrapper on behalf of the agent.
    PackageManager {
        /// ID of the agent that was installed.
        agent_id: String,
        /// Which package-manager strategy was used.
        method: InstallMethod,
        /// Package identifier handed to the installer.
        package: String,
    },
}

impl std::fmt::Display for InstallOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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

        assert_eq!(
            error.to_string(),
            "agent \"demo\" does not have an installable distribution"
        );
    }
}

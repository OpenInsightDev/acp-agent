use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::commands::install::{
    download_archive, extract_archive, make_executable, resolve_cmd_path,
};
use crate::registry::{BinaryTarget, Environment, Platform, RegistryAgent};

#[derive(Debug, Clone, PartialEq, Eq)]
/// A concrete instruction set for invoking an agent binary or package runner.
///
/// `program` is the executable path (binary or package manager), `args` are the
/// command line arguments already resolved by the runtime, `env` holds any
/// injected environment overrides, and `current_dir` is the working directory
/// (only set when a binary distribution was unpacked locally).
pub struct CommandSpec {
    /// The executable (or package manager) that will be launched.
    pub program: OsString,
    /// Arguments prepopulated by the runtime before user args are appended.
    pub args: Vec<OsString>,
    /// Environment overrides that should be injected into the child process.
    pub env: Vec<(OsString, OsString)>,
    /// Optional working directory used by the child process (set for binaries).
    pub current_dir: Option<PathBuf>,
}

#[derive(Debug)]
/// The prepared payload returned to transports and runners.
///
/// Consumers should drop this struct only after the agent process exits; the
/// optional `temp_dir` keeps the temporary extraction directory alive for the
/// duration of the child process.
pub struct PreparedCommand {
    /// The command specification suitable for `tokio::process::Command`.
    pub spec: CommandSpec,
    /// Temp directory that must survive while the agent process runs (if present).
    pub temp_dir: Option<tempfile::TempDir>,
}

/// Builds a `PreparedCommand` from a registry entry and extra user arguments.
///
/// The runtime first attempts to download and extract a binary distribution for
/// the current platform, then falls back to `npx` or `uvx` package runners if no
/// binary target exists. Any resolved temporary directory is returned so callers
/// can keep it alive while the spawned process runs.
pub async fn prepare_agent_command(
    agent: &RegistryAgent,
    user_args: &[String],
) -> Result<PreparedCommand> {
    if let Some(binary) = &agent.distribution.binary {
        let platform = Platform::current()?;
        if let Some(target) = binary.for_platform(platform) {
            let temp_dir = tokio::task::spawn_blocking(tempfile::tempdir)
                .await
                .context("failed to create temporary directory task")?
                .context("failed to create temporary directory")?;
            let archive_path = download_archive(target, temp_dir.path()).await?;
            let extracted_dir = temp_dir.path().join("extracted");
            tokio::fs::create_dir_all(&extracted_dir)
                .await
                .with_context(|| format!("failed to create {}", extracted_dir.display()))?;
            extract_archive(archive_path, extracted_dir.clone()).await?;

            let executable_path = resolve_cmd_path(&extracted_dir, &target.cmd);
            let metadata = tokio::fs::metadata(&executable_path).await;
            if metadata
                .as_ref()
                .map(|metadata| !metadata.is_file())
                .unwrap_or(true)
            {
                bail!(
                    "downloaded {}, but could not find \"{}\" at {}",
                    target.archive,
                    target.cmd,
                    executable_path.display()
                );
            }

            make_executable(&executable_path).await.with_context(|| {
                format!("failed to mark {} executable", executable_path.display())
            })?;

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

    bail!(
        "agent \"{}\" does not have a runnable distribution",
        agent.id
    )
}

fn package_command_spec(
    program: &str,
    package: &str,
    args: Option<&Vec<String>>,
    env: Option<&Environment>,
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

fn binary_command_spec(
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

fn clone_env_pairs(env: Option<&Environment>) -> Vec<(OsString, OsString)> {
    env.into_iter()
        .flat_map(|pairs| pairs.iter())
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect()
}

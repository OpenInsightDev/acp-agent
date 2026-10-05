use std::ffi::OsStr;
use std::process::{Child, Command as StdCommand, ExitStatus, Stdio};

use serde_json::Value;

use super::{BINARY, Harness};

/// One `acp-agent` invocation whose environment is cleared, so an assertion can
/// only observe what the harness provided.
pub struct Command {
    inner: StdCommand,
    display: String,
    stdin: Option<Vec<u8>>,
}

impl Command {
    pub(crate) fn new(harness: &Harness, args: &[&str]) -> Self {
        let mut inner = StdCommand::new(BINARY);
        inner.env_clear();
        inner.env("HOME", harness.home());
        inner.env("TMPDIR", harness.temp_dir());
        inner.env("PATH", harness.path_env());
        inner.env("ACP_AGENT_DAEMON_SOCKET", harness.socket_path());
        inner.env("RUST_BACKTRACE", "1");
        inner.args(args);
        inner.stdin(Stdio::null());
        Self {
            inner,
            display: format!("acp-agent {}", args.join(" ")),
            stdin: None,
        }
    }

    /// Feeds `input` to the child's standard input and closes it.
    ///
    /// Standard input is null by default, so a case that drives interactive
    /// input or passes a stream through to an agent opts in here.
    pub fn stdin_str(mut self, input: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(input.into());
        self
    }

    pub fn env(mut self, key: &str, value: impl AsRef<OsStr>) -> Self {
        self.inner.env(key, value);
        self
    }

    /// Removes an inherited variable the harness set, so a case can exercise
    /// the CLI's behaviour when it is absent.
    pub fn env_remove(mut self, key: &str) -> Self {
        self.inner.env_remove(key);
        self
    }

    /// Spawns the CLI without waiting for it.
    ///
    /// Only for a process a test keeps alive itself (the daemon); running a
    /// command to completion goes through [`Self::output`].
    pub fn spawn(mut self, stdout: Stdio, stderr: Stdio) -> Child {
        let display = self.display.clone();
        self.inner
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .unwrap_or_else(|error| panic!("failed to spawn `{display}`: {error}"))
    }

    /// Runs the CLI to completion and captures its output.
    ///
    /// Waiting asynchronously is what keeps a test's runtime free to serve the
    /// harness fixture server that a spawned CLI fetches from: a blocking wait
    /// would starve that server and hang the command forever.
    pub async fn output(mut self) -> CliOutput {
        let display = self.display.clone();
        let input = self.stdin.take();
        if input.is_some() {
            self.inner.stdin(Stdio::piped());
        }
        let mut child = tokio::process::Command::from(self.inner)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("failed to run `{display}`: {error}"));
        if let Some(input) = input {
            use tokio::io::AsyncWriteExt;

            let mut stdin = child.stdin.take().expect("standard input was piped");
            stdin
                .write_all(&input)
                .await
                .unwrap_or_else(|error| panic!("failed to write `{display}`'s stdin: {error}"));
            // Dropping the handle closes the pipe, so the child sees end of input.
        }
        let output = child
            .wait_with_output()
            .await
            .unwrap_or_else(|error| panic!("failed to run `{display}`: {error}"));
        CliOutput {
            command: display,
            status: output.status,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

#[derive(Debug)]
pub struct CliOutput {
    command: String,
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl CliOutput {
    pub fn exit_code(&self) -> i32 {
        self.status.code().unwrap_or(-1)
    }

    pub fn success(&self) -> &Self {
        assert!(
            self.status.success(),
            "expected `{}` to succeed\n{}",
            self.command,
            self.describe()
        );
        self
    }

    pub fn failure(&self) -> &Self {
        assert!(
            !self.status.success(),
            "expected `{}` to fail\n{}",
            self.command,
            self.describe()
        );
        self
    }

    pub fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|error| {
            panic!(
                "`{}` did not print JSON: {error}\n{}",
                self.command,
                self.describe()
            )
        })
    }

    pub fn lines(&self) -> Vec<&str> {
        self.stdout
            .lines()
            .filter(|line| !line.is_empty())
            .collect()
    }

    pub fn stdout_contains(&self, needle: &str) -> &Self {
        assert!(
            self.stdout.contains(needle),
            "`{}` did not print {needle:?}\n{}",
            self.command,
            self.describe()
        );
        self
    }

    pub fn stderr_contains(&self, needle: &str) -> &Self {
        assert!(
            self.stderr.contains(needle),
            "`{}` did not report {needle:?}\n{}",
            self.command,
            self.describe()
        );
        self
    }

    pub fn describe(&self) -> String {
        format!(
            "exit code: {}\nstdout:\n{}\nstderr:\n{}",
            self.exit_code(),
            self.stdout.trim_end_matches('\n'),
            self.stderr.trim_end_matches('\n')
        )
    }
}

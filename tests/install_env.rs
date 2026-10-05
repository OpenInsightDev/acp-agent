//! End-to-end tests for [`InstallEnv.md`](InstallEnv.md).
//!
//! Every case runs the CLI with a fixture `PATH`: the fake programs below are
//! written into the harness `bin` directory, so detection follows exactly which
//! of them exists and nothing reaches the network.
//!
//! | fake | stands in for |
//! | --- | --- |
//! | `deno` | the JavaScript toolchain, present |
//! | `npm` | the JavaScript toolchain, present without deno |
//! | `uv` | the Python toolchain, present |
//! | `curl` | the installer fetch: prints a stub installer that "installs" a toolchain under `HOME` and appends the fetched URL to a log |

mod harness;

use harness::{CliOutput, Harness};

const DENO_STUB: &str = "#!/bin/sh\necho \"deno 2.1.0 (stable, release, aarch64-apple-darwin)\"\n";
const NPM_STUB: &str = "#!/bin/sh\necho \"10.8.0\"\n";
const UV_STUB: &str = "#!/bin/sh\necho \"uv 0.5.0\"\n";

/// The fixture `bin` directory alone decides which toolchains exist.
///
/// The harness `PATH` also carries the inherited one, where a developer's own
/// `npm`, `deno`, or `uv` would otherwise satisfy detection; the standard system
/// directories stay so the fake installer scripts still find `sh` and `printf`.
fn fixture_path(harness: &Harness) -> String {
    format!(
        "{}:/usr/bin:/bin:/usr/sbin:/sbin",
        harness.bin_dir().display()
    )
}

/// The `install-env` invocation, with `args` appended.
fn install_env(harness: &Harness, args: &[&str]) -> harness::Command {
    let mut argv = vec!["install-env"];
    argv.extend_from_slice(args);
    harness.command(&argv).env("PATH", fixture_path(harness))
}

/// Runs `install-env`, feeding `stdin` to the confirmation prompt.
///
/// An empty `stdin` closes the input, which is the end-of-input answer.
async fn run_install_env(harness: &Harness, args: &[&str], stdin: &str) -> CliOutput {
    install_env(harness, args)
        .stdin_str(stdin.to_string())
        .output()
        .await
}

/// Writes the fake programs named in `tools` into the fixture `bin` directory.
fn write_fakes(harness: &Harness, tools: &[&str]) {
    for tool in tools {
        match *tool {
            "deno" => {
                harness.write_script("deno", DENO_STUB);
            }
            "npm" => {
                harness.write_script("npm", NPM_STUB);
            }
            "uv" => {
                harness.write_script("uv", UV_STUB);
            }
            "curl" => curl_stub(harness),
            other => panic!("unknown fake program: {other}"),
        };
    }
}

/// Writes the fake `curl`: it records the URL it was asked for and prints an
/// installer that creates a toolchain binary under `$HOME`, the way the real
/// installer scripts do.
fn curl_stub(harness: &Harness) {
    let log = curl_log(harness);
    let body = format!(
        r##"#!/bin/sh
url=""
for argument in "$@"; do
  case "$argument" in http*) url="$argument";; esac
done
printf '%s\n' "$url" >> '{log}'
case "$url" in
*deno.land*)
  printf '%s\n' 'mkdir -p "$HOME/.deno/bin"'
  printf '%s\n' 'printf "#!/bin/sh\necho deno 2.1.0\n" > "$HOME/.deno/bin/deno"'
  printf '%s\n' 'chmod +x "$HOME/.deno/bin/deno"'
  printf '%s\n' 'echo "deno was installed successfully"'
  ;;
*astral.sh*)
  printf '%s\n' 'mkdir -p "$HOME/.local/bin"'
  printf '%s\n' 'printf "#!/bin/sh\necho uv 0.5.0\n" > "$HOME/.local/bin/uv"'
  printf '%s\n' 'chmod +x "$HOME/.local/bin/uv"'
  printf '%s\n' 'echo "uv was installed successfully"'
  ;;
*)
  echo "fake curl: unexpected url: $url" >&2
  exit 22
  ;;
esac
"##,
        log = log.display()
    );
    harness.write_script("curl", &body);
}

/// Where the fake `curl` records the URLs it fetched.
fn curl_log(harness: &Harness) -> std::path::PathBuf {
    harness.fixtures_dir().join("curl.log")
}

/// Both toolchain binaries the fake `curl` installs.
fn installed_paths(harness: &Harness) -> Vec<std::path::PathBuf> {
    vec![
        harness.home().join(".deno/bin/deno"),
        harness.home().join(".local/bin/uv"),
    ]
}

/// The detection report, as `install-env` prints it.
fn report(harness: &Harness, found: &[&str]) -> String {
    let line = |tool: &str| {
        if found.contains(&tool) {
            format!(
                "{tool}: available ({})",
                harness.bin_dir().join(tool).display()
            )
        } else {
            format!("{tool}: missing")
        }
    };
    format!(
        "Environment detection results:\nJavaScript tools:\n{}\n{}\nPython tools:\n{}\n",
        line("npm"),
        line("deno"),
        line("uv")
    )
}

const DENO_PLAN: &str = "deno: sh -c \"curl -fsSL https://deno.land/install.sh | sh\"";
const UV_PLAN: &str = "uv: sh -c \"curl -LsSf https://astral.sh/uv/install.sh | sh\"";

fn assert_curl_not_run(harness: &Harness) {
    assert!(
        !curl_log(harness).exists(),
        "the installer must not be fetched"
    );
}

mod detection {
    use crate::harness::Harness;
    use crate::{
        UV_PLAN, assert_curl_not_run, installed_paths, report, run_install_env, write_fakes,
    };

    #[tokio::test]
    async fn reports() {
        let harness = Harness::new();
        write_fakes(&harness, &["deno", "npm"]);
        let output = run_install_env(&harness, &[], "n\n").await;
        output.success();
        assert_eq!(
            output.stdout,
            format!(
                "{}\nPlanned installation:\n{UV_PLAN}\nProceed with installation? [Y/n]: Installation cancelled.\n",
                report(&harness, &["deno", "npm"])
            )
        );
    }

    #[tokio::test]
    async fn satisfied() {
        let harness = Harness::new();
        write_fakes(&harness, &["deno", "uv"]);
        let output = run_install_env(&harness, &[], "").await;
        output.success();
        assert_eq!(
            output.stdout,
            format!(
                "{}\nEnvironment already satisfies the requirements. No installation is needed.\n",
                report(&harness, &["deno", "uv"])
            )
        );
        assert_curl_not_run(&harness);
        for path in installed_paths(&harness) {
            assert!(!path.exists(), "{}", path.display());
        }
    }
}

mod plan {
    use crate::harness::Harness;
    use crate::{DENO_PLAN, UV_PLAN, report, run_install_env, write_fakes};

    #[tokio::test]
    async fn missing() {
        let harness = Harness::new();
        let output = run_install_env(&harness, &[], "n\n").await;
        output.success();
        assert_eq!(
            output.stdout,
            format!(
                "{}\nPlanned installation:\n{DENO_PLAN}\n{UV_PLAN}\nProceed with installation? [Y/n]: Installation cancelled.\n",
                report(&harness, &[])
            )
        );
    }

    #[tokio::test]
    async fn skips_present() {
        let python_present = Harness::new();
        write_fakes(&python_present, &["uv"]);
        let output = run_install_env(&python_present, &[], "n\n").await;
        output.success();
        output.stdout_contains(DENO_PLAN);
        assert!(
            !output.stdout.contains(UV_PLAN),
            "a present toolchain is not planned:\n{}",
            output.stdout
        );

        let javascript_present = Harness::new();
        write_fakes(&javascript_present, &["npm"]);
        let output = run_install_env(&javascript_present, &[], "n\n").await;
        output.success();
        output.stdout_contains(UV_PLAN);
        assert!(
            !output.stdout.contains(DENO_PLAN),
            "npm satisfies the JavaScript toolchain, so deno is not planned:\n{}",
            output.stdout
        );
    }
}

mod confirmation {
    use crate::harness::Harness;
    use crate::{assert_curl_not_run, installed_paths, run_install_env, write_fakes};

    #[tokio::test]
    async fn declines() {
        let harness = Harness::new();
        write_fakes(&harness, &["curl"]);
        let output = run_install_env(&harness, &[], "n\n").await;
        output.success();
        output.stdout_contains("Proceed with installation? [Y/n]: Installation cancelled.");
        assert_curl_not_run(&harness);
        for path in installed_paths(&harness) {
            assert!(!path.exists(), "{}", path.display());
        }
    }

    #[tokio::test]
    async fn end_of_input() {
        let harness = Harness::new();
        write_fakes(&harness, &["curl"]);
        let output = run_install_env(&harness, &[], "").await;
        output.success();
        output.stdout_contains("Starting installation...");
    }

    #[tokio::test]
    async fn yes() {
        let harness = Harness::new();
        write_fakes(&harness, &["curl"]);
        let output = run_install_env(&harness, &["--yes"], "").await;
        output.success();
        output.stdout_contains("Starting installation...");
        assert!(
            !output.stdout.contains("Proceed with installation?"),
            "--yes prints no prompt:\n{}",
            output.stdout
        );
    }
}

mod install {
    use crate::harness::Harness;
    use crate::{curl_log, installed_paths, run_install_env, write_fakes};

    #[tokio::test]
    async fn installs() {
        let harness = Harness::new();
        write_fakes(&harness, &["curl"]);
        let output = run_install_env(&harness, &["--yes"], "").await;
        output.success();

        let deno = harness.home().join(".deno/bin/deno");
        let uv = harness.home().join(".local/bin/uv");
        let expected = format!(
            "Environment detection results:\nJavaScript tools:\nnpm: missing\ndeno: missing\nPython tools:\nuv: missing\n\nPlanned installation:\ndeno: sh -c \"curl -fsSL https://deno.land/install.sh | sh\"\nuv: sh -c \"curl -LsSf https://astral.sh/uv/install.sh | sh\"\n\nStarting installation...\n\ndeno installed and verified at {deno}\nNote: deno was installed outside the current PATH. Open a new shell if the command is not yet recognized.\nuv installed and verified at {uv}\nNote: uv was installed outside the current PATH. Open a new shell if the command is not yet recognized.\nEnvironment installation complete.\n",
            deno = deno.display(),
            uv = uv.display()
        );
        assert_eq!(output.stdout, expected);
        assert_eq!(
            std::fs::read_to_string(curl_log(&harness)).expect("the fake curl ran"),
            "https://deno.land/install.sh\nhttps://astral.sh/uv/install.sh\n"
        );
        for path in installed_paths(&harness) {
            assert!(path.is_file(), "{}", path.display());
        }
    }

    #[tokio::test]
    async fn path_note() {
        let harness = Harness::new();
        write_fakes(&harness, &["curl"]);
        let widened = format!(
            "{deno}:{uv}:{fixture}",
            deno = harness.home().join(".deno/bin").display(),
            uv = harness.home().join(".local/bin").display(),
            fixture = crate::fixture_path(&harness)
        );
        let output = harness
            .command(&["install-env", "--yes"])
            .env("PATH", widened)
            .stdin_str("")
            .output()
            .await;
        output.success();
        assert!(
            !output.stdout.contains("outside the current PATH"),
            "a toolchain installed into the PATH needs no note:\n{}",
            output.stdout
        );
    }

    #[tokio::test]
    async fn missing_curl() {
        let harness = Harness::new();
        // Only the empty fixture `bin` directory is on `PATH`, because a system
        // `curl` would otherwise satisfy the installer fetch.
        let output = harness
            .command(&["install-env"])
            .env("PATH", harness.bin_dir().display().to_string())
            .stdin_str("")
            .output()
            .await;
        assert_eq!(output.failure().exit_code(), 1, "\n{}", output.describe());
        output.stdout_contains("Starting installation...");
        assert!(
            !output.stdout.contains("Environment installation complete."),
            "a failed installation reports no completion:\n{}",
            output.stdout
        );
        // The harness sets `RUST_BACKTRACE` for diagnostics, so the chain is
        // asserted as the leading lines rather than the whole stream.
        let stderr: Vec<&str> = output.stderr.lines().collect();
        assert_eq!(
            &stderr[..4],
            [
                "Error: failed to install environment dependencies",
                "",
                "Caused by:",
                "    Cannot install deno because curl is not available in the current environment",
            ],
            "\n{}",
            output.describe()
        );
    }
}

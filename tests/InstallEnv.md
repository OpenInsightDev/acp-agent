# Install Env

`acp-agent install-env` ensures the local toolchains the other commands need are present: a JavaScript toolchain, satisfied by `npm` or `deno`, and the Python toolchain `uv`.
It fetches an upstream installer with `curl` and pipes it to `sh`, so a run that installs anything needs network access.
The command commits to these outcomes, in this order:

- It MUST report what it detected before changing anything. (`detection::reports`)
- It MUST stop once the requirements are satisfied. (`detection::satisfied`)
- It MUST plan only the toolchains that are missing. (`plan::missing`, `plan::skips_present`)
- It MUST install only after the run is confirmed, unless `--yes` was given. (`confirmation::declines`, `confirmation::end_of_input`, `confirmation::yes`)
- It MUST install and verify each planned toolchain. (`install::installs`, `install::path_note`)
- It MUST report an installer it cannot run. (`install::missing_curl`)

Report, plan, prompt, and progress go to standard output; failures go to standard error.

## Detection

`acp-agent install-env [--yes]` MUST print the detection report first and MUST name, for each toolchain, whether it was found and where.

| Line | Presence | Value |
| --- | --- | --- |
| `Environment detection results:` | always | Report heading. |
| `JavaScript tools:` | always | Group heading; the `npm` and `deno` lines follow it, in that order. |
| `npm: available (<path>)` | always | `PATH`-resolved `npm`, or `npm: missing`. |
| `deno: available (<path>)` | always | `PATH`-resolved `deno`, or `deno: missing`. |
| `Python tools:` | always | Group heading; the `uv` line follows it. |
| `uv: available (<path>)` | always | `PATH`-resolved `uv`, or `uv: missing`. |

A blank line ends the report.
A run whose requirements are met MUST print `Environment already satisfies the requirements. No installation is needed.` after that blank line and MUST exit `0` without fetching or installing anything. (`detection::satisfied`)

```text
$ acp-agent install-env
Environment detection results:
JavaScript tools:
npm: available (/tmp/example/tools/npm)
deno: available (/tmp/example/tools/deno)
Python tools:
uv: available (/tmp/example/tools/uv)

Environment already satisfies the requirements. No installation is needed.
```

## Plan

When a requirement is missing, the command MUST print `Planned installation:` after the report's blank line, followed by one line per missing toolchain.
A toolchain that is already available MUST NOT be planned, even when a different toolchain satisfies the same requirement: with `npm` present, `deno` is not planned. (`plan::skips_present`)

The planned lines are:

| Toolchain | Line |
| --- | --- |
| `deno` | `deno: sh -c "curl -fsSL https://deno.land/install.sh | sh"` |
| `uv` | `uv: sh -c "curl -LsSf https://astral.sh/uv/install.sh | sh"` |

With nothing installed, that block is:

```text
$ acp-agent install-env
Planned installation:
deno: sh -c "curl -fsSL https://deno.land/install.sh | sh"
uv: sh -c "curl -LsSf https://astral.sh/uv/install.sh | sh"
```

## Confirmation

Without `--yes`, the command MUST print the prompt `Proceed with installation? [Y/n]: ` and read one line from standard input.
An answer beginning with `n` MUST end the run with `Installation cancelled.` on the same line as the prompt, exit `0`, and no toolchain installed or fetched. (`confirmation::declines`)
End of input MUST be read as the default answer, so the run proceeds. (`confirmation::end_of_input`)
`--yes` MUST skip the prompt and proceed. (`confirmation::yes`)

Declining ends the run like this:

```text
$ acp-agent install-env
Proceed with installation? [Y/n]: Installation cancelled.
```

## Installation

A confirmed run MUST print `Starting installation...`, then a blank line, then one line per toolchain it installed.
Each installed toolchain MUST be verified by running it, and MUST be reported as `<toolchain> installed and verified at <path>`. (`install::installs`)
A toolchain installed outside the current `PATH` MUST be followed by `Note: <toolchain> was installed outside the current PATH. Open a new shell if the command is not yet recognized.`; a toolchain installed into a directory that is already on `PATH` MUST NOT be followed by it. (`install::path_note`)
The run MUST end with `Environment installation complete.` and MUST exit `0`.

The toolchains install below `$HOME`: `deno` at `~/.deno/bin/deno` and `uv` at `~/.local/bin/uv`.

```text
$ acp-agent install-env --yes
Environment detection results:
JavaScript tools:
npm: missing
deno: missing
Python tools:
uv: missing

Planned installation:
deno: sh -c "curl -fsSL https://deno.land/install.sh | sh"
uv: sh -c "curl -LsSf https://astral.sh/uv/install.sh | sh"

Starting installation...

deno installed and verified at /tmp/example/home/.deno/bin/deno
Note: deno was installed outside the current PATH. Open a new shell if the command is not yet recognized.
uv installed and verified at /tmp/example/home/.local/bin/uv
Note: uv was installed outside the current PATH. Open a new shell if the command is not yet recognized.
Environment installation complete.
```

An installer that cannot run MUST fail the run before any toolchain is installed: exit `1`, the failure on standard error, and no completion line on standard output. (`install::missing_curl`)
The failure for a missing `curl` is:

```text
$ acp-agent install-env --yes
Error: failed to install environment dependencies

Caused by:
    Cannot install deno because curl is not available in the current environment
```

## Out of scope and open questions

How `run`, `serve`, and the package runners use these toolchains is specified by [Run.md](Run.md) and [InstallPackage.md](InstallPackage.md).
Whether `acp-agent install-env` should also install Node.js where only `deno` is missing, and whether the required toolchain set is fixed at one JavaScript and one Python toolchain, are open questions.

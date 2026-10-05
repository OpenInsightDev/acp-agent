# InstallPackage

`acp-agent` runs npm and uvx package distributions without downloading an archive, and it commits to four outcomes:

- `install` prepares a package distribution through the runner that will later execute it, and reports what it prepared.
- `run` executes the package through that same runner with the registry's arguments and environment.
- `uninstall` removes the package through that runner and reports what it removed.
- install, run, and uninstall share one runner decision, so a package is prepared, executed, and removed through the same tool.

Binary distributions belong to [`InstallBinary.md`](InstallBinary.md), and catalog loading to [`Registry.md`](Registry.md).

## Runner decision

A registry agent declares an npm (`npx`) or Python (`uvx`) package, and one decision selects the runner that both prepares and executes it.

| Distribution | Runner | Execution program | Preparation program |
| --- | --- | --- | --- |
| npm (`npx`), with `npm` on `PATH` | npm | `npm` | `npm` |
| npm (`npx`), without `npm` | Deno | `deno` | `deno` |
| Python (`uvx`) | uvx | `uvx` | `uv` |

An npm distribution MUST select the npm runner when an executable `npm` is on `PATH`, and the Deno runner otherwise. (`runner::npm`, `runner::deno`)
A uvx distribution MUST select the uvx runner. (`runner::uvx`)
Install and run MUST resolve the runner through this one decision, so the cache install prepares is exactly what run later reads. (`runner::npm`, `runner::deno`)
A binary distribution takes priority over both package distributions and never reaches a package runner.

### Tests

- `runner::npm`: with `npm` available, an npm distribution prepares and executes through `npm`, asserting the exact program and arguments of each.
- `runner::deno`: without `npm`, the same distribution prepares and executes through `deno`, asserting the exact program and arguments of each.
- `runner::uvx`: a uvx distribution prepares through `uv` and executes through `uvx`, asserting the exact program and arguments of each.

## install

`acp-agent install <agent>` prepares the agent's package through its runner's own cache. (`install::npm`, `install::deno`, `install::uvx`)
On success it MUST print exactly one message line to standard output and exit `0`.

| Runner | Preparation | Message |
| --- | --- | --- |
| npm | `npm install --global <package>` | `Installed <id> via npm: <package>` |
| Deno | `deno cache --minimum-dependency-age 0 npm:<package>` | `Prepared <id> via deno cache: <package>` |
| uvx | `uv tool install <package>` | `Installed <id> via uv: <package>` |

A package installation MUST NOT write an `acp-agent` cache entry, so `list --installed` reports only cached binaries. (`install::not_in_inventory`)
A failed preparation exits `1` and writes `failed to install agent "<id>": <reason>` to standard output, with no success line.

```text
$ acp-agent install mock-npx
Installed mock-npx via npm: @mock/alpha
```

### Tests

- `install::npm`: an npm install prints the npm success line.
- `install::deno`: a Deno install prints the Deno preparation line.
- `install::uvx`: a uvx install prints the uv success line.
- `install::not_in_inventory`: after a package install, `list --installed --json` still prints an empty array.

## run

`acp-agent run <agent> [-- <args>]` executes the package through its runner. (`run::args`, `run::env`)
The registry-declared `args` MUST be placed after the package and before the arguments the user supplies after `--`, and the `--` separator itself is never passed on.
The registry-declared `env` MUST be added to the package process's environment.

| Runner | Arguments after the execution program |
| --- | --- |
| npm | `exec -- <package> <registry args> <user args>` |
| Deno | `x --allow-all --minimum-dependency-age 0 <package> <registry args> <user args>` |
| uvx | `<package> <registry args> <user args>` |

The command exits with the status the package process exits with.

### Tests

- `run::args`: registry args precede the user arguments after `--`.
- `run::env`: the registry-declared environment reaches the package process.

## uninstall

`acp-agent uninstall <agent>` removes the agent's package through its runner. (`uninstall::npm`, `uninstall::uvx`)
On success it MUST print exactly one message line to standard output and exit `0`.

| Runner | Removal | Message |
| --- | --- | --- |
| npm | `npm uninstall --global <package>` | `Uninstalled <id> via npm: <package>` |
| Deno | none | `Nothing to uninstall for <id>: its package is cached by deno, which manages its own cache` |
| uvx | `uv tool uninstall <tool>` | `Uninstalled <id> via uv: <tool>` |

The npm runner MUST remove the package with `npm uninstall --global <package>`. (`uninstall::npm`)
It runs only when `npm` reports the package in its global package list, which is consulted first.
The uvx runner MUST remove the package with `uv tool uninstall <tool>`. (`uninstall::uvx`)
The Deno runner MUST NOT invoke a package manager, because Deno owns its npm cache and creates no launcher. (`uninstall::deno`)

```text
$ acp-agent uninstall mock-npx
Uninstalled mock-npx via npm: @mock/alpha
```

### Tests

- `uninstall::npm`: an installed npm package is removed with the npm uninstaller and the npm message.
- `uninstall::deno`: without `npm`, uninstall invokes no package manager and prints the Deno nothing-to-uninstall message.
- `uninstall::uvx`: a uvx package is removed with the uv tool uninstaller and the uv message.

## Out of scope

- Binary distributions, which take priority over packages and are specified in [`InstallBinary.md`](InstallBinary.md).
- `serve`, which resolves agents through the same runner but has its own transport behavior.
- Catalog loading and validation, specified in [`Registry.md`](Registry.md).

## Open questions

- The runner is re-decided on every command, so install and a later run or uninstall agree only while `PATH` is unchanged; if `npm` appears or disappears between them the prepared and used tools can differ, and a package installed earlier is not removed by a later command that resolves to a different runner.
- Install prepares a cache (`npm install --global`, `deno cache`, `uv tool install`) while execution can fetch on demand (`npm exec`, `deno x`, `uvx`), so preparation is a pre-fetch rather than a prerequisite for running.

# cargo-target-cleaner

A Cargo wrapper that automatically frees disk space before build commands. It runs
`cargo clean` when a target exceeds its size limit or free disk space falls below
its minimum, then forwards your command to Cargo.

## Setup

Requires Cargo and a supported shell. Install a prebuilt binary for Linux or macOS (x86_64 or
ARM64) using [cargo-binstall](https://github.com/cargo-bins/cargo-binstall):

```sh
cargo binstall --git https://github.com/anotherempty/cargo-target-cleaner cargo-target-cleaner
```

Or use the shell installer:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/anotherempty/cargo-target-cleaner/releases/latest/download/cargo-target-cleaner-installer.sh | sh
```

Ensure `$CARGO_HOME/bin` (normally `~/.cargo/bin`) is on `PATH`, then run:

```sh
cargo-target-cleaner init zsh
```

Replace `zsh` with the argument for your shell from the table below, then open a
new session:

| Shell | Argument | Integration file |
| --- | --- | --- |
| Bash | `bash` | `~/.bashrc` |
| Cmd + Clink 1.6.11+ | `cmd` | `%CLINK_PROFILE%/cargo-target-cleaner.lua`, otherwise `%LOCALAPPDATA%/clink/cargo-target-cleaner.lua` |
| Elvish 0.21+ | `elvish` | `<config>/elvish/rc.elv` |
| Fish | `fish` | `<config>/fish/conf.d/cargo-target-cleaner.fish` |
| Ion | `ion` | `<config>/ion/initrc` |
| Nushell | `nu` | `<config>/nushell/config.nu` |
| PowerShell 7.3+ | `powershell` | `$PROFILE.CurrentUserAllHosts` and adjacent `cargo-target-cleaner.ps1`, located using `pwsh` |
| Tcsh | `tcsh` | `~/.tcshrc`, or existing `~/.cshrc` if `.tcshrc` is absent |
| Xonsh | `xonsh` | `~/.xonshrc` |
| Zsh | `zsh` | `$ZDOTDIR/.zshrc`, otherwise `~/.zshrc` |

`<config>` uses `XDG_CONFIG_HOME` when set, otherwise `~/.config` on Unix or
`%APPDATA%` on Windows. Nushell defaults to `~/Library/Application Support` on macOS.
Bash login profiles must source `.bashrc`. Cmd integration applies to interactive
commands, not batch scripts. On Windows, build from source and set `HOME` to your
user directory; prebuilt Windows distributions are not yet provided.

Setup preserves the rest of your shell configuration and creates the default global
cleanup config if missing. Existing `cargo` functions or aliases may be replaced; remove conflicting
aliases in Bash, Zsh, or PowerShell first. IDEs invoking Cargo directly are unaffected.
To remove integration, run `cargo-target-cleaner uninstall <shell>` and open a new session.
The binary and cleanup configs remain installed.

## Configuration

Global settings live in `~/.cargo-target-cleaner.toml`. Setup creates only the
`[free_space]` settings shown below; the other settings are optional:

```toml
verbose = false
max_target_size_gib = 20

[free_space]
min_free_space_gib = 100
across_projects = true
```

1. `max_target_size_gib`: clean the current target when its size exceeds this limit.
2. `free_space.min_free_space_gib`: clean when free space on the target directory's
   filesystem falls below this minimum.
3. `free_space.across_projects`: clean the largest known target on the current
   target's filesystem when `true`, or only the current project's target when
   `false`. Both free-space fields are required.
4. `verbose`: show cleanup progress and timings on stderr; defaults to `false`.

Specify thresholds as positive whole numbers in GiB. Omit `[free_space]` to disable
free-space cleanup and project registration. Existing config files are preserved during setup.

For a project-specific size limit, add `.cargo-target-cleaner.toml` at the project root:

```toml
max_target_size_gib = 10
verbose = true
```

The nearest project configuration determines the size limit instead of the global
configuration; an empty file disables size-based cleanup. `verbose` inherits the
global setting when omitted.
`[free_space]` is global-only and cannot be set or disabled by a project config.

## How it works

1. Before `build`, `check`, `test`, `run`, `bench`, `doc`, `clippy`, `rustc`, or
   `rustdoc`, the wrapper asks Cargo for the workspace and target directory.
   Standard single-letter aliases are supported; other commands pass through.
2. With cross-project cleanup enabled, existing targets are recorded in
   `~/.cargo-target-cleaner/projects.toml`, grouped by target filesystem. Projects are
   discovered as you use the wrapper, without scanning your disk. Each registry
   update refreshes filesystem identities, removes missing projects or targets,
   and deduplicates shared target directories. The refreshed registry stays in memory
   until both cleanup policies succeed, then is merged with concurrent registrations
   and saved atomically. Failed cleanup leaves the registry unchanged.
3. The free-space policy runs first, then the target-size policy. Each policy cleans
   at most one target directory. Cleanup removes **all Cargo artifacts in the selected target**,
   including other profiles or projects sharing it. Source files remain intact.
   Each `cargo clean` process has a 10-minute timeout. On timeout, the wrapper
   terminates and waits for that process before forwarding Cargo; partial deletions
   are not rolled back. Cross-project cleanup attempts to restore target symlink
   destinations even after cleanup errors or timeouts. This timeout does not cover metadata, filesystem scans,
   or registry locking.
4. The original command runs with its arguments and exit status preserved.
   Configuration, measurement, registry, or cleanup errors are reported as warnings;
   the original Cargo command still runs.

### Cleanup across disks

Cross-project cleanup selects targets only on the current target's filesystem.
Symlinks are resolved; missing targets use their nearest existing ancestor.
If no eligible target exists, cleanup is skipped. Cleaning one target does not
guarantee enough space for the next build.

## Development

Run shell and CLI integration tests with Docker Compose, including Bash, Elvish, Fish, Ion,
Nushell, PowerShell, Tcsh, Xonsh, and Zsh:

```sh
docker compose run --build --rm test
```

The source is mounted read-only; Docker volumes cache Cargo dependencies and build
artifacts. The first run builds the test image.

On macOS or Windows, install the relevant shells and run
`cargo test --locked --test shells --test cli`.

Run unit and cleanup tests, formatting, and Clippy locally:

```sh
cargo test --bin cargo-target-cleaner --test cleanup
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
```

## License

Licensed under either the [MIT license](LICENSE-MIT) or the
[Apache License, Version 2.0](LICENSE-APACHE), at your option.

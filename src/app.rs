use std::ffi::OsString;
use std::io::{self, Write};
use std::process::{Command, ExitCode};

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

use crate::error::Error;

#[derive(Parser)]
#[command(
    name = "cargo-target-cleaner",
    version,
    about = "Automatic per-project Cargo cleanup"
)]
pub struct App {
    #[command(subcommand)]
    command: Option<CliCommand>,
}

impl App {
    pub fn run() -> Result<ExitCode, Error> {
        let app = Self::parse();
        match app.command {
            Some(CliCommand::Init { shell }) => {
                crate::cleanup::Config::setup()?;
                Self::print(&shell.integration().setup()?)
            }
            Some(CliCommand::Uninstall { shell }) => Self::print(&shell.integration().remove()?),
            Some(CliCommand::Exec { args }) => Self::forward(&args),
            None => {
                Self::command().print_help().map_err(Error::Help)?;
                Ok(ExitCode::SUCCESS)
            }
        }
    }

    fn print(text: &str) -> Result<ExitCode, Error> {
        io::stdout()
            .lock()
            .write_all(text.as_bytes())
            .map_err(Error::Output)?;
        Ok(ExitCode::SUCCESS)
    }

    fn forward(args: &[OsString]) -> Result<ExitCode, Error> {
        if let Err(error) = crate::cleanup::Cleanup::before_command(args) {
            tracing::warn!(
                "cargo-target-cleaner: {error}; continuing with requested Cargo command"
            );
        }
        let mut command = Command::new("cargo");
        command.args(args);

        // Execute the PATH executable, never the shell function. Replacing this
        // process preserves Cargo's signals, exit status, and standard streams.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            Err(Error::Forward(command.exec()))
        }

        #[cfg(not(unix))]
        {
            let status = command.status().map_err(Error::Forward)?;
            let code = status.code().ok_or(Error::MissingExitCode)?;
            std::process::exit(code);
        }
    }
}

#[derive(Subcommand)]
enum CliCommand {
    /// Install shell integration and create the global cleanup config if missing.
    Init {
        #[arg(value_enum)]
        shell: ShellKind,
    },
    /// Remove shell integration from the shell configuration file.
    Uninstall {
        #[arg(value_enum)]
        shell: ShellKind,
    },
    /// Run Cargo, checking cleanup thresholds before build commands.
    Exec {
        #[arg(last = true, value_name = "CARGO_ARGS")]
        args: Vec<OsString>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ShellKind {
    Xonsh,
    Tcsh,
    #[value(name = "powershell", alias = "pwsh")]
    PowerShell,
    Nu,
    Ion,
    Fish,
    Elvish,
    Cmd,
    Bash,
    Zsh,
}

impl ShellKind {
    fn integration(&self) -> &dyn crate::shell::Shell {
        match self {
            Self::Cmd => &crate::shell::Cmd,
            Self::Elvish => &crate::shell::Elvish,
            Self::Fish => &crate::shell::Fish,
            Self::Ion => &crate::shell::Ion,
            Self::Nu => &crate::shell::Nu,
            Self::PowerShell => &crate::shell::PowerShell,
            Self::Tcsh => &crate::shell::Tcsh,
            Self::Xonsh => &crate::shell::Xonsh,
            Self::Bash => &crate::shell::Bash,
            Self::Zsh => &crate::shell::Zsh,
        }
    }
}

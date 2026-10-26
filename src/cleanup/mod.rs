mod filesystem;
mod free_space;
mod registry;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::error::{CargoOperation, Error, FileOperation};
use free_space::{FreeSpaceConfig, FreeSpacePolicy};
use registry::Project;

pub struct Cleanup;

impl Cleanup {
    pub fn before_command(args: &[OsString]) -> Result<(), Error> {
        let Some(invocation) = Invocation::parse(args) else {
            return Ok(());
        };
        let cwd = std::env::current_dir().map_err(Error::CurrentDirectory)?;
        let manifest = invocation.manifest.as_ref().map(|path| cwd.join(path));
        let start = manifest.as_deref().and_then(Path::parent).unwrap_or(&cwd);
        let settings = Config::find(start)?;
        if settings.min_free.is_none() && settings.max_target.is_none() {
            return Ok(());
        }

        let _guard = tracing::subscriber::set_default(crate::diagnostics::Diagnostics::subscriber(
            settings.verbose,
        ));
        tracing::info!(target: crate::diagnostics::Diagnostics::PROGRESS_TARGET, "cargo-target-cleaner: resolving Cargo workspace for cleanup checks");
        let workspace = invocation.workspace()?;
        let target_override = invocation.target.as_ref().map(|path| cwd.join(path));
        let target = target_override
            .as_deref()
            .unwrap_or(&workspace.target_directory);
        let registry = match &settings.min_free {
            Some(policy) => policy.apply(&invocation, &workspace, target)?,
            None => None,
        };
        if let Some(limit) = settings.max_target
            && Self::directory_exceeds(target, limit)?
        {
            Self::clean(invocation.command(CargoOperation::Clean), target)?;
        }
        if let Some(registry) = registry {
            registry.persist()?;
        }
        tracing::info!(target: crate::diagnostics::Diagnostics::PROGRESS_TARGET, "cargo-target-cleaner: cleanup checks complete; forwarding Cargo command");
        Ok(())
    }

    fn clean(mut command: Command, target: &Path) -> Result<(), Error> {
        let started = std::time::Instant::now();
        tracing::info!(
            "cargo-target-cleaner: running cargo clean ({})",
            target.display()
        );
        let status = command
            .arg("--target-dir")
            .arg(target)
            .status()
            .map_err(|source| Error::Execute {
                operation: CargoOperation::Clean,
                source,
            })?;
        if !status.success() {
            return Err(Error::CleanFailed(status));
        }
        tracing::info!(target: crate::diagnostics::Diagnostics::PROGRESS_TARGET, target = %target.display(), elapsed_seconds = started.elapsed().as_secs_f64(), "cargo-target-cleaner: cargo clean completed");
        Ok(())
    }

    fn directory_exceeds(root: &Path, limit: ByteLimit) -> Result<bool, Error> {
        tracing::info!(target: crate::diagnostics::Diagnostics::PROGRESS_TARGET, target = %root.display(), limit_bytes = limit.get(), "cargo-target-cleaner: checking target size limit");
        let exceeds = Project::target_size(root, Some(limit.get()))? > limit.get();
        tracing::info!(target: crate::diagnostics::Diagnostics::PROGRESS_TARGET, target = %root.display(), exceeds_limit = exceeds, "cargo-target-cleaner: target size check complete");
        Ok(exceeds)
    }
}

pub(crate) struct Config;

impl Config {
    const FILE_NAME: &str = ".cargo-target-cleaner.toml";
    // Keep 100 GiB available by default for Cargo builds and other applications.
    const DEFAULT_MIN_FREE_GIB: u64 = 100;

    fn home_path() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(|home| PathBuf::from(home).join(Self::FILE_NAME))
    }

    pub(crate) fn setup() -> Result<(), Error> {
        let path = Self::home_path().ok_or(Error::MissingHome)?;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => writeln!(
                file,
                "[free_space]\nmin_free_space_gib = {}\nacross_projects = true",
                Self::DEFAULT_MIN_FREE_GIB
            )
            .map_err(|error| Error::file(FileOperation::Write, &path, error)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // Report unreadable paths instead of treating directories as config files.
                fs::read_to_string(&path)
                    .map(|_| ())
                    .map_err(|error| Error::file(FileOperation::Read, &path, error))
            }
            Err(error) => Err(Error::file(FileOperation::Create, &path, error)),
        }
    }

    fn find(start: &Path) -> Result<Settings, Error> {
        let home = Self::home_path();
        let mut global = match &home {
            Some(path) => Self::read::<GlobalConfig>(path)?
                .map(GlobalConfig::into_settings)
                .transpose()
                .map_err(|source| Error::Policy {
                    path: path.to_owned(),
                    source,
                })?
                .unwrap_or_default(),
            None => Settings::default(),
        };
        let mut project = None;
        for directory in start.ancestors() {
            let path = directory.join(Self::FILE_NAME);
            if home.as_ref() == Some(&path) {
                break;
            }
            if let Some(config) = Self::read::<ProjectConfig>(&path)? {
                if let Some(verbose) = config.verbose {
                    global.verbose = verbose;
                }
                project = Some(
                    config
                        .into_max_target()
                        .map_err(|source| Error::Policy { path, source })?,
                );
                break;
            }
        }
        if let Some(project) = project {
            global.max_target = project;
        }
        Ok(global)
    }

    fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, Error> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Error::file(FileOperation::Read, path, error)),
        };
        toml::from_str(&contents)
            .map(Some)
            .map_err(|source| Error::Config {
                path: path.to_owned(),
                source,
            })
    }
}

// TOML boundary types encode which fields each configuration file permits.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobalConfig {
    verbose: Option<bool>,
    max_target_size_gib: Option<u64>,
    free_space: Option<FreeSpaceConfig>,
}

impl GlobalConfig {
    fn into_settings(self) -> Result<Settings, PolicyError> {
        Ok(Settings {
            verbose: self.verbose.unwrap_or(false),
            max_target: self
                .max_target_size_gib
                .map(|value| ByteLimit::from_gib(value, Threshold::MaxTarget))
                .transpose()?,
            min_free: self
                .free_space
                .map(FreeSpaceConfig::into_policy)
                .transpose()?,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectConfig {
    verbose: Option<bool>,
    max_target_size_gib: Option<u64>,
}

impl ProjectConfig {
    fn into_max_target(self) -> Result<Option<ByteLimit>, PolicyError> {
        self.max_target_size_gib
            .map(|value| ByteLimit::from_gib(value, Threshold::MaxTarget))
            .transpose()
    }
}

#[derive(Default)]
struct Settings {
    verbose: bool,
    max_target: Option<ByteLimit>,
    min_free: Option<FreeSpacePolicy>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ByteLimit(NonZeroU64);

impl ByteLimit {
    const BYTES_PER_GIB: u64 = 1 << 30;

    fn from_gib(value: u64, threshold: Threshold) -> Result<Self, PolicyError> {
        let gib = NonZeroU64::new(value).ok_or(PolicyError::Zero(threshold))?;
        gib.get()
            .checked_mul(Self::BYTES_PER_GIB)
            .and_then(NonZeroU64::new)
            .map(Self)
            .ok_or(PolicyError::TooLarge(threshold))
    }

    fn get(self) -> u64 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Threshold {
    MaxTarget,
    MinFree,
}

impl std::fmt::Display for Threshold {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::MaxTarget => "max_target_size_gib",
            Self::MinFree => "min_free_space_gib",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum PolicyError {
    #[error("{0} must be positive; omit it to disable")]
    Zero(Threshold),
    #[error("{0} is too large")]
    TooLarge(Threshold),
}

struct Invocation<'a> {
    toolchain: Option<&'a OsStr>,
    manifest: Option<&'a Path>,
    target: Option<&'a Path>,
    shared: Vec<&'a OsStr>,
}

impl<'a> Invocation<'a> {
    const METADATA_FORMAT: &'static str = "--format-version=1";

    fn parse(args: &'a [OsString]) -> Option<Self> {
        let mut result = Self {
            toolchain: None,
            manifest: None,
            target: None,
            shared: Vec::new(),
        };
        let mut args = args.iter().peekable();
        if args
            .peek()
            .is_some_and(|arg| arg.as_encoded_bytes().starts_with(b"+"))
        {
            result.toolchain = args.next().map(OsString::as_os_str);
        }
        let mut found_command = false;
        while let Some(arg) = args.next() {
            if arg == "--" {
                break;
            }
            if arg == "--help" || arg == "-h" || arg == "--version" || arg == "-V" {
                return None;
            }
            if arg == "--manifest-path" || arg == "-m" {
                let value = args.next()?;
                result.manifest = Some(Path::new(value));
            } else if arg == "--target-dir" {
                result.target = Some(Path::new(args.next()?));
            } else if arg == "--config" || arg == "-Z" || arg == "--color" {
                result.shared.push(arg.as_os_str());
                result.shared.push(args.next()?.as_os_str());
            } else if arg == "--locked" || arg == "--offline" || arg == "--frozen" {
                result.shared.push(arg.as_os_str());
            } else if let Some(text) = arg.to_str() {
                if let Some(value) = text.strip_prefix("--manifest-path=") {
                    result.manifest = Some(Path::new(value));
                } else if let Some(value) = text.strip_prefix("-m") {
                    result.manifest = Some(Path::new(value.strip_prefix('=').unwrap_or(value)));
                } else if let Some(value) = text.strip_prefix("--target-dir=") {
                    result.target = Some(Path::new(value));
                } else if text.starts_with("-C") {
                    // Cargo's unstable directory switch changes config resolution.
                    // Forward it untouched rather than clean the wrong workspace.
                    return None;
                } else if text.starts_with("--config=")
                    || text.starts_with("--color=")
                    || text.starts_with("-Z")
                {
                    result.shared.push(arg.as_os_str());
                } else if !found_command {
                    if matches!(
                        text,
                        "build"
                            | "b"
                            | "check"
                            | "c"
                            | "test"
                            | "t"
                            | "run"
                            | "r"
                            | "bench"
                            | "doc"
                            | "d"
                            | "clippy"
                            | "rustc"
                            | "rustdoc"
                    ) {
                        found_command = true;
                    } else if text != "--quiet"
                        && text != "--verbose"
                        && !(text.starts_with('-')
                            && text
                                .strip_prefix('-')
                                .is_some_and(|flags| flags.chars().all(|c| c == 'v' || c == 'q')))
                    {
                        return None;
                    }
                }
            } else if !found_command || arg.as_encoded_bytes().starts_with(b"-") {
                return None;
            }
        }
        found_command.then_some(result)
    }

    fn command(&self, operation: CargoOperation) -> Command {
        let mut command = Command::new("cargo");
        if let Some(toolchain) = &self.toolchain {
            command.arg(toolchain);
        }
        command.arg(operation.argument()).args(&self.shared);
        if let Some(manifest) = &self.manifest {
            command.arg("--manifest-path").arg(manifest);
        }
        command
    }

    fn workspace(&self) -> Result<Workspace, Error> {
        let output = self
            .command(CargoOperation::Metadata)
            .args([OsStr::new("--no-deps"), OsStr::new(Self::METADATA_FORMAT)])
            .output()
            .map_err(|source| Error::Execute {
                operation: CargoOperation::Metadata,
                source,
            })?;
        if !output.status.success() {
            return Err(Error::MetadataFailed {
                status: output.status,
                stderr: output.stderr,
            });
        }
        serde_json::from_slice(&output.stdout).map_err(Error::Metadata)
    }
}

#[derive(Deserialize)]
struct Workspace {
    workspace_root: PathBuf,
    target_directory: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::{ByteLimit, GlobalConfig, PolicyError, ProjectConfig, Threshold};

    #[test]
    fn byte_limits_reject_zero_and_overflow_for_each_threshold() {
        const MAX_GIB: u64 = u64::MAX / ByteLimit::BYTES_PER_GIB;
        const OVERFLOW_GIB: u64 = MAX_GIB + 1;
        for threshold in [Threshold::MaxTarget, Threshold::MinFree] {
            assert_eq!(
                ByteLimit::from_gib(0, threshold),
                Err(PolicyError::Zero(threshold))
            );
            assert_eq!(
                ByteLimit::from_gib(OVERFLOW_GIB, threshold),
                Err(PolicyError::TooLarge(threshold))
            );
            assert_eq!(
                ByteLimit::from_gib(MAX_GIB, threshold).unwrap().get(),
                MAX_GIB * ByteLimit::BYTES_PER_GIB
            );
        }
    }

    #[test]
    fn global_settings_allow_both_thresholds() {
        let config: GlobalConfig = toml::from_str(
            "max_target_size_gib = 1\n[free_space]\nmin_free_space_gib = 1\nacross_projects = true",
        )
        .unwrap();
        let settings = config.into_settings().unwrap();
        assert!(settings.max_target.is_some());
        assert!(settings.min_free.is_some());
    }

    #[test]
    fn project_settings_reject_free_space_threshold() {
        let result = toml::from_str::<ProjectConfig>(
            "[free_space]\nmin_free_space_gib = 1\nacross_projects = true",
        );
        assert!(result.is_err());
    }
}

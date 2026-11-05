use std::fs::{File, OpenOptions};
use std::path::Path;

use tracing_subscriber::prelude::*;

use crate::error::{Error, FileOperation};

pub(crate) struct Diagnostics;

impl Diagnostics {
    pub(crate) const PROGRESS_TARGET: &str = "cargo_target_cleaner::progress";
    const LOG_FILE: &str = ".cargo-target-cleaner.log";

    pub(crate) fn subscriber(verbose: bool) -> impl tracing::Subscriber + Send + Sync {
        let (file, error) = match Self::open_log() {
            Ok(file) => (Some(file), None),
            Err(error) => (None, Some(error)),
        };
        let subscriber = Self::with_file(verbose, file);
        if let Some(error) = error {
            tracing::subscriber::with_default(Self::stderr(), || {
                tracing::warn!("cargo-target-cleaner: {error}; file logging is unavailable");
            });
        }
        subscriber
    }

    pub(crate) fn stderr() -> impl tracing::Subscriber + Send + Sync {
        Self::with_file(false, None)
    }

    fn open_log() -> Result<File, Error> {
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .ok_or(Error::MissingLogHome)?;
        Self::open_path(&Path::new(&home).join(Self::LOG_FILE))
    }

    fn open_path(path: &Path) -> Result<File, Error> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|source| Error::file(FileOperation::Create, path, source))
    }

    fn with_file(verbose: bool, file: Option<File>) -> impl tracing::Subscriber + Send + Sync {
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::stderr)
                    .without_time()
                    .with_ansi(false)
                    .with_target(false)
                    .with_filter(tracing_subscriber::filter::filter_fn(move |metadata| {
                        *metadata.level() <= tracing::Level::INFO
                            && (verbose || metadata.target() != Self::PROGRESS_TARGET)
                    })),
            )
            .with(file.map(|file| {
                tracing_subscriber::fmt::layer()
                    .with_writer(file)
                    .with_ansi(false)
                    .with_filter(tracing_subscriber::filter::LevelFilter::INFO)
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::Diagnostics;

    #[test]
    fn file_records_progress_when_quiet_and_preserves_previous_runs() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(Diagnostics::LOG_FILE);
        const PREVIOUS: &str = "previous run\n";
        const PROGRESS: &str = "cleanup progress test";
        const WARNING: &str = "cleanup warning test";
        std::fs::write(&path, PREVIOUS).unwrap();
        let subscriber =
            Diagnostics::with_file(false, Some(Diagnostics::open_path(&path).unwrap()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: Diagnostics::PROGRESS_TARGET, "{PROGRESS}");
            tracing::warn!("{WARNING}");
        });
        let contents = std::fs::read_to_string(path).unwrap();
        assert!(contents.starts_with(PREVIOUS));
        assert!(contents.contains(PROGRESS));
        assert!(contents.contains(WARNING));
        assert!(!contents.contains('\u{1b}'));
    }
}

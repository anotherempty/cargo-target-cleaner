use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

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

    fn open_log() -> Result<LineLog, Error> {
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .ok_or(Error::MissingLogHome)?;
        Self::open_path(&Path::new(&home).join(Self::LOG_FILE))
    }

    fn open_path(path: &Path) -> Result<LineLog, Error> {
        LineLog::new(path).map_err(|source| Error::file(FileOperation::Create, path, source))
    }

    fn with_file(verbose: bool, file: Option<LineLog>) -> impl tracing::Subscriber + Send + Sync {
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

// The stable sidecar lock protects reading, trimming, and appending across
// processes. Reopen the log for each event because trimming replaces the file.
struct LineLog {
    path: PathBuf,
    lock_path: PathBuf,
    failure_reported: AtomicBool,
    failure_dispatch: tracing::Dispatch,
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LineLog {
    type Writer = &'a Self;

    fn make_writer(&'a self) -> Self::Writer {
        self
    }
}

impl Write for &LineLog {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        match self.write_event(buffer) {
            Ok(()) => {
                self.failure_reported.store(false, Ordering::Relaxed);
                Ok(buffer.len())
            }
            Err(error) => {
                if !self.failure_reported.swap(true, Ordering::Relaxed) {
                    self.report_failure(&error);
                }
                Err(error)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        // Writes go directly to File; there is no userspace buffer to flush.
        Ok(())
    }
}

impl LineLog {
    const RETAINED_LINES: usize = 10_000;
    const TRIM_AT_LINES: usize = 11_000;

    fn new(path: &Path) -> io::Result<Self> {
        let log = Self {
            path: path.to_owned(),
            lock_path: Self::suffixed(path, ".lock"),
            failure_reported: AtomicBool::new(false),
            failure_dispatch: tracing::Dispatch::new(Diagnostics::stderr()),
        };
        let _lock = log.lock()?;
        log.open()?;
        Ok(log)
    }

    fn report_failure(&self, error: &io::Error) {
        use tracing::callsite::Callsite;

        // Dispatch directly: replacing the thread-local subscriber from inside
        // its event callback would reborrow tracing's dispatcher and panic.
        let callsite = tracing::callsite! {
            name: "file logging failure",
            kind: tracing::metadata::Kind::EVENT,
            level: tracing::Level::WARN,
            fields: message
        };
        let metadata = callsite.metadata();
        let message = format_args!(
            "cargo-target-cleaner: file logging failed for {}: {error}; retrying on later events",
            self.path.display()
        );
        let values: &[Option<&dyn tracing::field::Value>] = &[Some(&message)];
        self.failure_dispatch.event(&tracing::Event::new_child_of(
            None,
            metadata,
            &metadata.fields().value_set_all(values),
        ));
    }

    fn write_event(&self, buffer: &[u8]) -> io::Result<()> {
        let _lock = self.lock()?;
        let mut file = self.open()?;
        let mut lines = VecDeque::new();
        let mut trimmed = false;
        let mut retention_triggered = false;
        {
            // Count physical lines, including an unterminated final line. Chain
            // the event onto the file so a partial line is counted correctly.
            let mut reader = BufReader::new((&mut file).chain(buffer));
            loop {
                let mut line = Vec::new();
                if reader.read_until(b'\n', &mut line)? == 0 {
                    break;
                }
                if line.iter().all(u8::is_ascii_whitespace) {
                    trimmed = true;
                    continue;
                }
                lines.push_back(line);
                if lines.len() >= Self::TRIM_AT_LINES {
                    retention_triggered = true;
                }
                if retention_triggered {
                    while lines.len() > Self::RETAINED_LINES {
                        lines.pop_front();
                    }
                    trimmed = true;
                }
            }
        }
        if !trimmed {
            return file.write_all(buffer);
        }

        // Replace only after the retained lines have been written successfully.
        // A failed rewrite leaves the previous log intact; no archives remain.
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut replacement = tempfile::NamedTempFile::new_in(parent)?;
        replacement
            .as_file()
            .set_permissions(file.metadata()?.permissions())?;
        {
            let mut writer = BufWriter::new(&mut replacement);
            for line in lines {
                writer.write_all(&line)?;
            }
            writer.flush()?;
        }
        drop(file);
        replacement
            .persist(&self.path)
            .map_err(|error| error.error)?;
        Ok(())
    }

    fn suffixed(path: &Path, suffix: &str) -> PathBuf {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    }

    fn lock(&self) -> io::Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.lock_path)?;
        fs2::FileExt::lock_exclusive(&file)?;
        Ok(file)
    }

    fn open(&self) -> io::Result<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::{Diagnostics, LineLog};

    #[test]
    fn retains_latest_lines_at_boundary_and_for_oversized_events() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(Diagnostics::LOG_FILE);
        let log = LineLog::new(&path).unwrap();
        const OLD: &[u8] = b"old\n";
        const NEW: &[u8] = b"new\n";
        (&log)
            .write_all(&OLD.repeat(LineLog::TRIM_AT_LINES - 1))
            .unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            OLD.repeat(LineLog::TRIM_AT_LINES - 1)
        );
        (&log).write_all(NEW).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            [OLD.repeat(LineLog::RETAINED_LINES - 1), NEW.to_vec()].concat()
        );
        (&log).write_all(NEW).unwrap();
        const NEW_LINES: usize = 2;
        assert_eq!(
            std::fs::read(&path).unwrap(),
            [
                OLD.repeat(LineLog::RETAINED_LINES - 1),
                NEW.repeat(NEW_LINES)
            ]
            .concat()
        );
        (&log)
            .write_all(&NEW.repeat(LineLog::TRIM_AT_LINES + 1))
            .unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            NEW.repeat(LineLog::RETAINED_LINES)
        );
        const EXPECTED_FILES: usize = 2; // Log and coordination lock, no archives.
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            EXPECTED_FILES
        );
    }

    #[test]
    fn removes_blank_lines_and_preserves_unterminated_non_utf8_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(Diagnostics::LOG_FILE);
        let log = LineLog::new(&path).unwrap();
        const PARTIAL: &[u8] = b"partial";
        const ENDING: &[u8] = b" end\r\n\n \t\r\n\xff";
        const RETAINED_ENDING: &[u8] = b" end\r\n\xff";
        (&log)
            .write_all(&b"old\n".repeat(LineLog::RETAINED_LINES - 1))
            .unwrap();
        (&log).write_all(PARTIAL).unwrap();
        (&log).write_all(ENDING).unwrap();
        const RETAINED_OLD_LINES: usize = LineLog::RETAINED_LINES - 1;
        assert_eq!(
            std::fs::read(&path).unwrap(),
            [
                b"old\n".repeat(RETAINED_OLD_LINES),
                PARTIAL.to_vec(),
                RETAINED_ENDING.to_vec()
            ]
            .concat()
        );
    }

    #[test]
    fn removes_existing_and_incoming_blank_lines_below_limit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(Diagnostics::LOG_FILE);
        std::fs::write(&path, b"\nold\n \t\r\n").unwrap();
        let log = LineLog::new(&path).unwrap();
        (&log).write_all(b"\r\nnew\n\n \t").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"old\nnew\n");
        (&log).write_all(b"\n\r\n \t").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"old\nnew\n");
    }

    #[test]
    fn concurrent_writers_preserve_every_new_line_while_trimming() {
        const WRITERS: usize = 4;
        const RECORDS_PER_WRITER: usize = 20;
        const RECORD: &[u8] = b"concurrent record\n";
        const OLD: &[u8] = b"old\n";
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(Diagnostics::LOG_FILE);
        std::fs::write(&path, OLD.repeat(LineLog::TRIM_AT_LINES - 1)).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..WRITERS {
                let path = &path;
                scope.spawn(move || {
                    let log = LineLog::new(path).unwrap();
                    for _ in 0..RECORDS_PER_WRITER {
                        (&log).write_all(RECORD).unwrap();
                    }
                });
            }
        });
        const NEW_RECORDS: usize = WRITERS * RECORDS_PER_WRITER;
        assert_eq!(
            std::fs::read(&path).unwrap(),
            [
                OLD.repeat(LineLog::RETAINED_LINES - 1),
                RECORD.repeat(NEW_RECORDS)
            ]
            .concat()
        );
    }

    #[test]
    fn subscriber_reports_failure_once_and_recovers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(Diagnostics::LOG_FILE);
        let warnings = directory.path().join("warnings.log");
        let mut log = LineLog::new(&path).unwrap();
        log.failure_dispatch = tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .with_writer(std::fs::File::create(&warnings).unwrap())
                .with_ansi(false)
                .finish(),
        );
        let saved = directory.path().join("saved.log");
        let subscriber = Diagnostics::with_file(false, Some(log));
        const WARNING: &str = "file logging failed";
        const RECOVERED: &str = "logging recovered";
        const FAILURE_EPISODES: usize = 2;
        const EVENTS_PER_FAILURE: usize = 3;
        tracing::subscriber::with_default(subscriber, || {
            for episode in 1..=FAILURE_EPISODES {
                let previous = std::fs::read(&path).unwrap();
                std::fs::rename(&path, &saved).unwrap();
                std::fs::create_dir(&path).unwrap();
                for _ in 0..EVENTS_PER_FAILURE {
                    tracing::info!(target: Diagnostics::PROGRESS_TARGET, "event during failure");
                }
                assert_eq!(std::fs::read(&saved).unwrap(), previous);
                assert_eq!(
                    std::fs::read_to_string(&warnings)
                        .unwrap()
                        .matches(WARNING)
                        .count(),
                    episode
                );
                std::fs::remove_dir(&path).unwrap();
                std::fs::rename(&saved, &path).unwrap();
                tracing::info!(target: Diagnostics::PROGRESS_TARGET, "{RECOVERED}");
                assert!(std::fs::read_to_string(&path).unwrap().contains(RECOVERED));
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn replacement_write_failure_preserves_log_and_recovers() {
        const CHILD_ENV: &str = "CARGO_TARGET_CLEANER_WRITE_FAILURE_CHILD";
        const TEST_NAME: &str =
            "diagnostics::tests::replacement_write_failure_preserves_log_and_recovers";
        const WARNING: &str = "file logging failed";
        const EXPECTED_WARNINGS: usize = 1;
        if std::env::var_os(CHILD_ENV).is_none() {
            // Resource limits and signal dispositions are process-wide. Run
            // only this test in a child so parallel tests remain unaffected.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST_NAME, "--nocapture"])
                .env(CHILD_ENV, "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            assert_eq!(
                String::from_utf8_lossy(&output.stderr)
                    .matches(WARNING)
                    .count(),
                EXPECTED_WARNINGS
            );
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(Diagnostics::LOG_FILE);
        const OLD: &[u8] = b"old\n";
        const RECOVERED: &str = "replacement write recovered";
        const FAILED_EVENTS: usize = 2;
        const MAX_FILE_BYTES: libc::rlim_t = 1;
        const SYSCALL_SUCCESS: libc::c_int = 0;
        const EXPECTED_FILES: usize = 2; // Active log and lock; no leaked temporaries.
        let original = OLD.repeat(LineLog::TRIM_AT_LINES - 1);
        std::fs::write(&path, &original).unwrap();
        let subscriber = Diagnostics::with_file(false, Some(LineLog::new(&path).unwrap()));
        let mut previous = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: the rlimit pointer is valid. Signal handling and limits are
        // changed only in this isolated child; SIG_IGN is a valid disposition.
        unsafe {
            assert_eq!(
                libc::getrlimit(libc::RLIMIT_FSIZE, &mut previous),
                SYSCALL_SUCCESS
            );
            assert_ne!(libc::signal(libc::SIGXFSZ, libc::SIG_IGN), libc::SIG_ERR);
            let restricted = libc::rlimit {
                rlim_cur: MAX_FILE_BYTES,
                rlim_max: previous.rlim_max,
            };
            assert_eq!(
                libc::setrlimit(libc::RLIMIT_FSIZE, &restricted),
                SYSCALL_SUCCESS
            );
        }
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..FAILED_EVENTS {
                tracing::info!(target: Diagnostics::PROGRESS_TARGET, "trigger replacement write");
                assert_eq!(std::fs::read(&path).unwrap(), original);
                assert_eq!(
                    std::fs::read_dir(directory.path()).unwrap().count(),
                    EXPECTED_FILES
                );
            }
            // SAFETY: previous contains the limit read above and remains valid.
            unsafe {
                assert_eq!(
                    libc::setrlimit(libc::RLIMIT_FSIZE, &previous),
                    SYSCALL_SUCCESS
                );
            }
            tracing::info!(target: Diagnostics::PROGRESS_TARGET, "{RECOVERED}");
        });
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), LineLog::RETAINED_LINES);
        assert!(contents.starts_with(&"old\n".repeat(LineLog::RETAINED_LINES - 1)));
        assert!(contents.lines().last().unwrap().contains(RECOVERED));
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            EXPECTED_FILES
        );
    }

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

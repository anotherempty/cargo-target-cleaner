#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use unix::Group;
#[cfg(windows)]
use windows::Group;

use super::supervisor::Cancellation;
use crate::error::Error;

pub(super) struct ProcessTree {
    child: Child,
    group: Group,
}

impl ProcessTree {
    const POLL_INTERVAL: Duration = Duration::from_millis(20);

    pub(super) fn spawn(mut command: Command) -> Result<Self, Error> {
        // The worker's stdin is a lifetime pipe, never input for Cargo/shims.
        command.stdin(Stdio::null());
        let (child, group) = Group::spawn(command).map_err(Error::ProcessSupervision)?;
        Ok(Self { child, group })
    }

    pub(super) fn wait(
        mut self,
        timeout: Duration,
        cancellation: &Cancellation,
    ) -> Result<ExitStatus, Error> {
        let started = Instant::now();
        let result = loop {
            if let Err(error) = cancellation.check() {
                break Err(error);
            }
            match self.child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(error) => break Err(Error::CleanWait(error)),
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break Err(Error::CleanTimeout(timeout));
            }
            std::thread::sleep(remaining.min(Self::POLL_INTERVAL));
        };
        // Even a successful shim may leave descendants behind. Stop all of
        // them before restoration, releasing the lock, or forwarding Cargo.
        self.stop();
        result
    }

    fn stop(&mut self) {
        // Never release the cleanup lock when termination cannot be confirmed.
        // An OS error is reported and retried while this supervisor stays alive.
        loop {
            match self.group.terminate() {
                Ok(()) => break,
                Err(error) => tracing::warn!(
                    "failed to terminate cleanup process group: {error}; retaining cleanup lock"
                ),
            }
            std::thread::sleep(Self::POLL_INTERVAL);
        }
        loop {
            match self.child.wait() {
                Ok(_) => break,
                Err(error) => tracing::warn!(
                    "failed to reap cleanup process: {error}; retaining cleanup lock"
                ),
            }
            std::thread::sleep(Self::POLL_INTERVAL);
        }
        loop {
            match self.group.finished() {
                Ok(true) => break,
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    "failed to confirm cleanup process group exit: {error}; retaining cleanup lock"
                ),
            }
            std::thread::sleep(Self::POLL_INTERVAL);
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    const TEST_TIMEOUT: Duration = Duration::from_millis(20);
    const READY_TIMEOUT: Duration = Duration::from_secs(5);
    const CHILD_SLEEP_SECONDS: &str = "30";
    const FAILURE_EXIT_CODE: i32 = 9;

    struct Fixture {
        directory: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                directory: tempfile::tempdir().unwrap(),
            }
        }

        fn pid_path(&self) -> PathBuf {
            self.directory.path().join("descendant")
        }

        fn spawn(&self, wait: bool) -> ProcessTree {
            let mut command = Command::new("sh");
            command
                .args([
                    "-c",
                    if wait {
                        "sleep \"$2\" & echo $! > \"$1\"; wait"
                    } else {
                        "sleep \"$2\" & echo $! > \"$1\"; exit 0"
                    },
                    "shim",
                ])
                .arg(self.pid_path())
                .arg(CHILD_SLEEP_SECONDS);
            ProcessTree::spawn(command).unwrap()
        }

        fn descendant(&self) -> libc::pid_t {
            let start = Instant::now();
            loop {
                if let Ok(text) = fs::read_to_string(self.pid_path())
                    && let Ok(pid) = text.trim().parse()
                {
                    return pid;
                }
                assert!(
                    start.elapsed() < READY_TIMEOUT,
                    "shim did not start descendant"
                );
                std::thread::sleep(ProcessTree::POLL_INTERVAL);
            }
        }

        fn assert_gone(pid: libc::pid_t) {
            // SAFETY: signal zero only checks whether this test process exists.
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
    }

    #[test]
    fn timeout_stops_and_reaps_shim_and_descendant() {
        let fixture = Fixture::new();
        let tree = fixture.spawn(true);
        let leader = libc::pid_t::try_from(tree.child.id()).unwrap();
        let descendant = fixture.descendant();
        let result = tree.wait(TEST_TIMEOUT, &Cancellation::default());
        assert!(matches!(result, Err(Error::CleanTimeout(timeout)) if timeout == TEST_TIMEOUT));
        Fixture::assert_gone(leader);
        Fixture::assert_gone(descendant);
    }

    #[test]
    fn successful_shim_cannot_leave_cleanup_running() {
        let fixture = Fixture::new();
        let tree = fixture.spawn(false);
        let descendant = fixture.descendant();
        assert!(
            tree.wait(READY_TIMEOUT, &Cancellation::default())
                .unwrap()
                .success()
        );
        Fixture::assert_gone(descendant);
    }

    #[test]
    fn clean_exit_status_is_preserved() {
        for exit_code in [0, FAILURE_EXIT_CODE] {
            let mut command = Command::new("sh");
            command
                .args(["-c", "exit \"$1\"", "clean"])
                .arg(exit_code.to_string());
            let status = ProcessTree::spawn(command)
                .unwrap()
                .wait(READY_TIMEOUT, &Cancellation::default())
                .unwrap();
            assert_eq!(status.code(), Some(exit_code));
        }
    }
}

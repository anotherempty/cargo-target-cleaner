use std::ffi::OsString;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::Error;

pub(super) struct Supervisor;

impl Supervisor {
    const WAIT_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
    pub(super) fn run(args: &[OsString]) -> Result<(), Error> {
        let executable = std::env::current_exe().map_err(Error::Supervisor)?;
        let mut command = Command::new(executable);
        command
            .arg("cleanup-worker")
            .arg("--")
            .args(args)
            .stdin(Stdio::piped());
        // Keep the worker alive when the terminal or caller kills the wrapper.
        // It owns the cleanup lock and observes wrapper death through pipe EOF.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
        }
        let mut worker = command.spawn().map_err(Error::Supervisor)?;
        // Child::wait normally closes stdin. Keep this handle until the worker
        // exits; its closure is our cancellation signal, not ordinary input.
        let mut lifeline = worker.stdin.take();
        let mut wait_error = None;
        let status = loop {
            match worker.wait() {
                Ok(status) => break status,
                Err(error) => {
                    tracing::warn!(
                        "failed to wait for cleanup worker: {error}; cancelling cleanup and waiting for it to stop"
                    );
                    wait_error.get_or_insert(error);
                    drop(lifeline.take());
                    std::thread::sleep(Self::WAIT_RETRY_INTERVAL);
                }
            }
        };
        drop(lifeline);
        if let Some(error) = wait_error {
            return Err(Error::Supervisor(error));
        }
        if status.success() {
            Ok(())
        } else {
            Err(Error::SupervisorFailed(status))
        }
    }
}

#[derive(Default)]
pub(super) struct Cancellation {
    cancelled: Arc<AtomicBool>,
}

impl Cancellation {
    pub(super) fn monitor_parent() -> Result<Self, Error> {
        let cancellation = Self::default();
        let cancelled = Arc::clone(&cancellation.cancelled);
        let dispatcher = tracing::dispatcher::get_default(Clone::clone);
        std::thread::Builder::new()
            .name("wrapper-lifetime".into())
            .spawn(move || {
                let _guard = tracing::dispatcher::set_default(&dispatcher);
                let mut input = std::io::stdin().lock();
                let mut byte = [0_u8];
                loop {
                    match input.read(&mut byte) {
                        Ok(0) => break,
                        Ok(_) => continue,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            tracing::warn!("cleanup supervisor lost its wrapper pipe: {error}");
                            break;
                        }
                    }
                }
                cancelled.store(true, Ordering::Release);
            })
            .map_err(Error::Supervisor)?;
        Ok(cancellation)
    }

    pub(super) fn check(&self) -> Result<(), Error> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(Error::CleanupCancelled)
        } else {
            Ok(())
        }
    }
}

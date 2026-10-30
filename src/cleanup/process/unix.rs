use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

pub(super) struct Group {
    id: libc::pid_t,
}

impl Group {
    pub(super) fn spawn(mut command: Command) -> io::Result<(Child, Self)> {
        #[cfg(target_os = "linux")]
        {
            // Adopt shim descendants so we can reap them after group termination.
            // This is set only in the dedicated cleanup worker.
            // SAFETY: prctl takes integer arguments for this operation.
            if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        let mut child = command.process_group(0).spawn()?;
        let id = match libc::pid_t::try_from(child.id()) {
            Ok(id) => id,
            Err(error) => {
                child.kill()?;
                child.wait()?;
                return Err(io::Error::new(io::ErrorKind::InvalidData, error));
            }
        };
        Ok((child, Self { id }))
    }

    pub(super) fn terminate(&self) -> io::Result<()> {
        // SAFETY: the positive PID was obtained from our child. Its negative
        // selects only the dedicated cleanup process group.
        if unsafe { libc::kill(-self.id, libc::SIGKILL) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }

    pub(super) fn finished(&self) -> io::Result<bool> {
        #[cfg(target_os = "linux")]
        loop {
            // The direct child has already been reaped via Child::wait. Reap
            // only adopted descendants in this group, never unrelated children.
            // SAFETY: null status is allowed, and WNOHANG never blocks.
            let result = unsafe { libc::waitpid(-self.id, std::ptr::null_mut(), libc::WNOHANG) };
            if result > 0 {
                continue;
            }
            if result == 0 {
                return Ok(false);
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            if error.raw_os_error() != Some(libc::ECHILD) {
                return Err(error);
            }
            break;
        }
        // On other Unix systems orphaned descendants are reaped by init.
        // SAFETY: signal zero checks existence without delivering a signal.
        if unsafe { libc::kill(-self.id, 0) } == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(true)
        } else {
            Err(error)
        }
    }
}

use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
};

pub(super) struct Group {
    job: OwnedHandle,
}

impl Group {
    const TERMINATED_EXIT_CODE: u32 = 1;
    const RESUME_FAILED: u32 = u32::MAX;

    pub(super) fn spawn(mut command: Command) -> io::Result<(Child, Self)> {
        let group = Self::new()?;
        // Suspend before executing any shim code, so it cannot spawn a child
        // before assignment to the job. The job disallows breakaway.
        let mut child = command.creation_flags(CREATE_SUSPENDED).spawn()?;
        let setup = group.assign(&child).and_then(|()| Self::resume(child.id()));
        if let Err(error) = setup {
            child.kill()?;
            child.wait()?;
            return Err(error);
        }
        Ok((child, group))
    }

    fn new() -> io::Result<Self> {
        // SAFETY: null attributes create a non-inheritable, unnamed job.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateJobObjectW returned a new valid handle we uniquely own.
        let job = unsafe { OwnedHandle::from_raw_handle(handle) };
        let group = Self { job };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the handle is live and the buffer has the specified structure/size.
        if unsafe {
            SetInformationJobObject(
                group.handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(group)
    }

    fn handle(&self) -> HANDLE {
        self.job.as_raw_handle()
    }

    fn assign(&self, child: &Child) -> io::Result<()> {
        // SAFETY: both handles remain open for this call.
        if unsafe { AssignProcessToJobObject(self.handle(), child.as_raw_handle()) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn resume(process_id: u32) -> io::Result<()> {
        // std::process::Child exposes the process handle, not its initial thread.
        // A newly created suspended process has only that initial thread.
        // SAFETY: snapshot creation takes flags and an ignored PID for this flag.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a successful snapshot returns an owned handle.
        let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
        let mut thread = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        // SAFETY: thread points to a correctly sized writable structure.
        let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut thread) };
        while found != 0 {
            if thread.th32OwnerProcessID == process_id {
                // SAFETY: open the enumerated thread with only resume permission.
                let handle = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread.th32ThreadID) };
                if handle.is_null() {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: OpenThread returned an owned handle.
                let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
                // SAFETY: the live handle has THREAD_SUSPEND_RESUME access.
                if unsafe { ResumeThread(handle.as_raw_handle()) } == Self::RESUME_FAILED {
                    return Err(io::Error::last_os_error());
                }
                return Ok(());
            }
            // SAFETY: the snapshot and writable entry remain valid.
            found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut thread) };
        }
        Err(io::Error::last_os_error())
    }

    pub(super) fn terminate(&self) -> io::Result<()> {
        // SAFETY: the job is owned and contains only this cleanup's processes.
        if unsafe { TerminateJobObject(self.handle(), Self::TERMINATED_EXIT_CODE) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn finished(&self) -> io::Result<bool> {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: the live job and correctly sized output buffer are valid.
        if unsafe {
            QueryInformationJobObject(
                self.handle(),
                JobObjectBasicAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(accounting.ActiveProcesses == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cleanup::process::ProcessTree;
    use crate::cleanup::supervisor::Cancellation;
    use crate::error::Error;
    use std::fs;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    const READY_TIMEOUT: Duration = Duration::from_secs(10);
    const CLEAN_TIMEOUT: Duration = Duration::from_millis(20);
    const POLL_INTERVAL: Duration = Duration::from_millis(20);

    #[test]
    fn timeout_stops_shim_and_descendant_in_job() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("descendant");
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-NonInteractive", "-Command",
            "$child = Start-Process powershell.exe -ArgumentList '-NoProfile -NonInteractive -Command Start-Sleep -Seconds 30' -PassThru; [System.IO.File]::WriteAllText($env:PID_FILE, [string]$child.Id); Wait-Process -Id $child.Id"])
            .env("PID_FILE", &pid_file);
        let tree = ProcessTree::spawn(command).unwrap();
        let start = Instant::now();
        let pid = loop {
            if let Ok(text) = fs::read_to_string(&pid_file)
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                break pid;
            }
            assert!(
                start.elapsed() < READY_TIMEOUT,
                "PowerShell shim did not start descendant"
            );
            std::thread::sleep(POLL_INTERVAL);
        };
        // SAFETY: request only wait access to the PID written by the test shim.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        assert!(!handle.is_null());
        // SAFETY: OpenProcess returned an owned handle.
        let descendant = unsafe { OwnedHandle::from_raw_handle(handle) };
        let result = tree.wait(CLEAN_TIMEOUT, &Cancellation::default());
        assert!(matches!(result, Err(Error::CleanTimeout(timeout)) if timeout == CLEAN_TIMEOUT));
        // SAFETY: the owned process handle stays valid; a zero timeout only polls.
        assert_eq!(
            unsafe { WaitForSingleObject(descendant.as_raw_handle(), 0) },
            WAIT_OBJECT_0
        );
    }
}

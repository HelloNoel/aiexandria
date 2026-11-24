//! Windows standard-handle isolation and crash-safe model ownership.
//! process-wrap's std JobObject does not enable kill-on-close. A second, nested
//! job ensures Windows reaps the model when its supervisor terminates, including
//! crashes. This handle is intentionally not inheritable by the model.
use std::io;
use std::os::windows::io::AsRawHandle;

use process_wrap::std::ChildWrapper;
use windows_sys::Win32::Foundation::{
    CloseHandle, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

pub(super) fn isolate_stdio() -> io::Result<()> {
    // Windows otherwise inherits the original CLI/MCP pipe handles even when
    // Command redirects the supervisor's standard streams to NUL. Keeping any
    // writer alive prevents the client from observing EOF after the CLI exits.
    // Clearing inheritance leaves our streams usable; Command duplicates the
    // handles explicitly requested by Stdio::inherit when spawning children.
    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: these standard handles are borrowed, never closed here. Only
        // their inheritance flag is changed, permanently and idempotently.
        unsafe {
            let handle = GetStdHandle(id);
            if !handle.is_null()
                && handle != INVALID_HANDLE_VALUE
                && SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

pub(super) struct KillJob(HANDLE);

impl KillJob {
    pub(super) fn attach(child: &dyn ChildWrapper) -> io::Result<Self> {
        let process = child
            .process_handle()
            .ok_or_else(|| io::Error::other("model process handle unavailable"))?;
        // SAFETY: null security/name pointers create an unnamed, non-inheritable
        // job. The returned handle is checked and owned by this RAII guard.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self(handle);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the information class, pointer and size describe the same
        // initialized structure. Both job and borrowed process handles are live.
        unsafe {
            if SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            if AssignProcessToJobObject(job.0, process.as_raw_handle()) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(job)
    }
}

impl Drop for KillJob {
    fn drop(&mut self) {
        // SAFETY: this guard uniquely owns the job handle. Closing it also
        // terminates any model descendants that have not exited normally.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

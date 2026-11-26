//! Job object that kills the worker when the UI process goes away.

use super::Handle;
use windows_sys::Win32::System::JobObjects::*;

pub struct Job(Handle);

impl Job {
    pub fn new() -> std::io::Result<Self> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let job = Job(Handle(handle));
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                job.0.raw(),
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(job)
    }

    pub fn assign(&self, child: &std::process::Child) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        if unsafe { AssignProcessToJobObject(self.0.raw(), child.as_raw_handle() as _) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

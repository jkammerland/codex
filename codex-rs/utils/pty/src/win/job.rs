use filedescriptor::OwnedHandle;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::RawHandle;
use std::sync::Mutex;
use tokio::process::Child;
use tokio::process::Command;
use winapi::shared::ntdef::NT_SUCCESS;
use winapi::shared::ntdef::NTSTATUS;
use winapi::shared::winerror::ERROR_INVALID_PARAMETER;
use winapi::shared::winerror::ERROR_MORE_DATA;
use winapi::um::jobapi::IsProcessInJob;
use winapi::um::jobapi2::AssignProcessToJobObject;
use winapi::um::jobapi2::CreateJobObjectW;
use winapi::um::jobapi2::QueryInformationJobObject;
use winapi::um::jobapi2::SetInformationJobObject;
use winapi::um::jobapi2::TerminateJobObject;
use winapi::um::processthreadsapi::OpenProcess;
use winapi::um::processthreadsapi::TerminateProcess;
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::winbase::CREATE_NO_WINDOW;
use winapi::um::winbase::CREATE_SUSPENDED;
use winapi::um::winbase::INFINITE;
use winapi::um::winbase::WAIT_FAILED;
use winapi::um::winbase::WAIT_OBJECT_0;
use winapi::um::winnt::HANDLE;
use winapi::um::winnt::JOB_OBJECT_LIMIT_BREAKAWAY_OK;
use winapi::um::winnt::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
use winapi::um::winnt::JOBOBJECT_BASIC_PROCESS_ID_LIST;
use winapi::um::winnt::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;
use winapi::um::winnt::JobObjectBasicProcessIdList;
use winapi::um::winnt::JobObjectExtendedLimitInformation;
use winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION;
use winapi::um::winnt::PROCESS_SET_QUOTA;
use winapi::um::winnt::PROCESS_SUSPEND_RESUME;
use winapi::um::winnt::PROCESS_TERMINATE;
use winapi::um::winnt::SYNCHRONIZE;

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtResumeProcess(process_handle: HANDLE) -> NTSTATUS;
}

/// Owns a Windows Job Object used to terminate a spawned process tree.
#[derive(Debug)]
pub struct JobObject {
    handle: OwnedHandle,
    // A mutex makes the state check, Job Object API call, and state update
    // atomic with respect to concurrent preserve and terminate requests.
    preserve_descendants: Mutex<bool>,
}

impl JobObject {
    /// Creates a Job Object configured to terminate all members when its last handle closes.
    pub fn create() -> io::Result<Self> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };

        Self::set_limit_flags(
            &handle,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK,
        )?;

        Ok(Self {
            handle,
            preserve_descendants: Mutex::new(false),
        })
    }

    /// Creates a Job Object whose owned descendants cannot explicitly break away.
    pub fn create_without_breakaway() -> io::Result<Self> {
        let job = Self::create()?;
        Self::set_limit_flags(&job.handle, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)?;
        Ok(job)
    }

    /// Terminates this owned job and waits on its processes' authoritative exit handles.
    /// Call from a blocking thread. The job must not have preserved descendants.
    pub fn terminate_and_wait(self) -> io::Result<()> {
        if *self
            .preserve_descendants
            .lock()
            .map_err(|_| io::Error::other("job state lock poisoned"))?
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot await termination of preserved descendants",
            ));
        }

        let mut previous_ids = Vec::new();
        loop {
            let process_ids = self.process_ids()?;
            if process_ids.is_empty() {
                return Ok(());
            }
            if process_ids == previous_ids {
                return Err(io::Error::other(
                    "job process list did not advance after exit",
                ));
            }
            let mut processes = Vec::with_capacity(process_ids.len());
            for &process_id in &process_ids {
                let handle = unsafe {
                    OpenProcess(
                        SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                        /*bInheritHandle*/ 0,
                        process_id,
                    )
                };
                if handle.is_null() {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                        // The process exited between the job snapshot and opening its handle.
                        continue;
                    }
                    return Err(error);
                }
                let handle = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };
                let mut member = 0;
                let queried = unsafe {
                    IsProcessInJob(
                        handle.as_raw_handle().cast(),
                        self.handle.as_raw_handle().cast(),
                        &mut member,
                    )
                };
                if queried == 0 {
                    return Err(io::Error::last_os_error());
                }
                // A reused numeric ID is not ownership evidence. Only await handles that
                // still belong to this job; a mismatch proves the original process exited.
                if member != 0 {
                    processes.push(handle);
                }
            }

            self.terminate()?;
            for process in processes {
                let waited =
                    unsafe { WaitForSingleObject(process.as_raw_handle().cast(), INFINITE) };
                match waited {
                    WAIT_OBJECT_0 => {}
                    WAIT_FAILED => return Err(io::Error::last_os_error()),
                    _ => return Err(io::Error::other("unexpected process wait result")),
                }
            }

            // Only an observed process exit advances the loop. Re-enumeration catches
            // descendants created between the snapshot and termination, without relying
            // on advisory job notifications or periodic queries.
            previous_ids = process_ids;
        }
    }

    fn process_ids(&self) -> io::Result<Vec<u32>> {
        // usize storage supplies the alignment of the variable-length ULONG_PTR array.
        let header_size = std::mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList);
        let mut storage = vec![0_usize; 16];
        loop {
            let buffer_size = u32::try_from(std::mem::size_of_val(storage.as_slice()))
                .map_err(|_| io::Error::other("job process list is too large"))?;
            let list = storage
                .as_mut_ptr()
                .cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
            let queried = unsafe {
                QueryInformationJobObject(
                    self.handle.as_raw_handle().cast(),
                    JobObjectBasicProcessIdList,
                    list.cast(),
                    buffer_size,
                    std::ptr::null_mut(),
                )
            };
            let error = (queried == 0).then(io::Error::last_os_error);
            let count = unsafe { (*list).NumberOfProcessIdsInList } as usize;
            let assigned = unsafe { (*list).NumberOfAssignedProcesses } as usize;
            if error
                .as_ref()
                .is_some_and(|error| error.raw_os_error() == Some(ERROR_MORE_DATA as i32))
                || count < assigned
            {
                let words = header_size.div_ceil(std::mem::size_of::<usize>())
                    + assigned.max(storage.len());
                storage.resize(words, 0);
                continue;
            }
            if let Some(error) = error {
                return Err(error);
            }
            let ids = unsafe {
                std::slice::from_raw_parts(
                    std::ptr::addr_of!((*list).ProcessIdList).cast::<usize>(),
                    count,
                )
            };
            let mut ids = ids
                .iter()
                .map(|id| {
                    u32::try_from(*id).map_err(|_| io::Error::other("invalid job process id"))
                })
                .collect::<io::Result<Vec<_>>>()?;
            ids.sort_unstable();
            return Ok(ids);
        }
    }

    /// Captures an owned process handle before its numeric identifier can be reused.
    pub fn open_process_handle(process_id: u32) -> io::Result<std::os::windows::io::OwnedHandle> {
        let handle = unsafe {
            OpenProcess(PROCESS_TERMINATE, /*bInheritHandle*/ 0, process_id)
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }

        Ok(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle.cast()) })
    }

    /// Terminates the exact process identified by a previously captured handle.
    pub fn terminate_process_handle(handle: &std::os::windows::io::OwnedHandle) -> io::Result<()> {
        let terminated = unsafe {
            TerminateProcess(handle.as_raw_handle().cast(), /*uExitCode*/ 1)
        };
        if terminated == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn set_limit_flags(handle: &OwnedHandle, flags: u32) -> io::Result<()> {
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = flags;
        let configured = unsafe {
            SetInformationJobObject(
                handle.as_raw_handle().cast(),
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of_mut!(limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Assigns a running process to this job.
    ///
    /// Assignment is not retroactive: descendants created before this call
    /// completes are not guaranteed to become members of the job.
    pub(crate) fn assign_process(&self, process_handle: RawHandle) -> io::Result<()> {
        let assigned = unsafe {
            AssignProcessToJobObject(self.handle.as_raw_handle().cast(), process_handle.cast())
        };
        if assigned == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Prepares a background child to be assigned to this job before it runs.
    ///
    /// Replaces all creation flags with `CREATE_SUSPENDED | CREATE_NO_WINDOW`.
    /// Interactive children that need to inherit a console require an explicit launch.
    pub fn prepare_suspended_spawn(&self, command: &mut Command) {
        command
            .creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED)
            .kill_on_drop(true);
    }

    /// Assigns and resumes a suspended child, returning whether assignment succeeded.
    ///
    /// Nested jobs can reject assignment. Such a child is resumed without
    /// containment so callers can preserve their existing compatibility fallback.
    pub fn assign_and_resume_process(&self, process_id: u32) -> io::Result<bool> {
        let process = unsafe {
            OpenProcess(
                PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_SUSPEND_RESUME,
                /*bInheritHandle*/ 0,
                process_id,
            )
        };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let process = unsafe { OwnedHandle::from_raw_handle(process.cast()) };
        let assignment = self.assign_process(process.as_raw_handle());

        let status = unsafe { NtResumeProcess(process.as_raw_handle().cast()) };
        if !NT_SUCCESS(status) {
            unsafe {
                TerminateProcess(process.as_raw_handle().cast(), /*uExitCode*/ 1)
            };
            return Err(io::Error::other(format!(
                "failed to resume suspended process: NTSTATUS {status:#x}"
            )));
        }

        match assignment {
            Ok(()) => Ok(true),
            Err(error) => {
                log::warn!(
                    "Windows process job assignment unavailable for pid {process_id}: {error}"
                );
                Ok(false)
            }
        }
    }

    /// Starts a console-free background process, falling back when containment is unavailable.
    /// The returned job owns the contained process tree when assignment succeeds.
    pub fn spawn_background(command: &mut Command) -> io::Result<(Child, Option<Self>)> {
        Self::spawn_background_with_job(command, Self::create())
    }

    fn spawn_background_with_job(
        command: &mut Command,
        job: io::Result<Self>,
    ) -> io::Result<(Child, Option<Self>)> {
        match job.and_then(|job| job.spawn_contained(command).map(|child| (child, job))) {
            Ok((child, job)) => Ok((child, Some(job))),
            Err(_) => {
                // Remove suspension before retrying, including after failed assignment.
                command.creation_flags(CREATE_NO_WINDOW).kill_on_drop(true);
                command.spawn().map(|child| (child, None))
            }
        }
    }

    /// Starts a background child without a console, assigning it to this job before it runs.
    ///
    /// Replaces existing creation flags as in [`Self::prepare_suspended_spawn`].
    pub fn spawn_contained(&self, command: &mut Command) -> io::Result<Child> {
        self.prepare_suspended_spawn(command);
        let child = command.spawn()?;
        self.assign_and_resume_child(&child)?;
        Ok(child)
    }

    /// Starts a contained child and drains it before returning any post-spawn error.
    /// Unlike the compatibility launcher, this never retries without containment.
    pub async fn spawn_contained_owned(&self, command: &mut Command) -> io::Result<Child> {
        self.prepare_suspended_spawn(command);
        let mut child = command.spawn()?;
        if let Err(error) = self.assign_and_resume_child(&child) {
            if let Err(kill_error) = child.start_kill()
                && child.try_wait()?.is_none()
            {
                return Err(kill_error);
            }
            child.wait_with_output().await?;
            return Err(error);
        }
        Ok(child)
    }

    fn assign_and_resume_child(&self, child: &Child) -> io::Result<()> {
        let process_handle = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("missing child process handle"))?;
        self.assign_process(process_handle)?;

        let status = unsafe { NtResumeProcess(process_handle.cast()) };
        if !NT_SUCCESS(status) {
            return Err(io::Error::other(format!(
                "failed to resume contained process: NTSTATUS {status:#x}"
            )));
        }

        Ok(())
    }

    /// Allows contained descendants to keep running after the root exits normally.
    ///
    /// This disables both explicit job termination and kill-on-close for this
    /// object. Calls race safely with [`Self::terminate`]: whichever operation
    /// acquires the state lock first determines whether the process tree is
    /// preserved or terminated.
    pub fn preserve_descendants(&self) -> io::Result<()> {
        let mut preserve_descendants = self
            .preserve_descendants
            .lock()
            .map_err(|_| io::Error::other("job state lock poisoned"))?;
        if *preserve_descendants {
            return Ok(());
        }

        Self::set_limit_flags(&self.handle, JOB_OBJECT_LIMIT_BREAKAWAY_OK)?;
        *preserve_descendants = true;
        Ok(())
    }

    /// Terminates every process currently assigned to the job.
    pub fn terminate(&self) -> io::Result<()> {
        let preserve_descendants = self
            .preserve_descendants
            .lock()
            .map_err(|_| io::Error::other("job state lock poisoned"))?;
        if *preserve_descendants {
            return Ok(());
        }

        let terminated = unsafe {
            TerminateJobObject(self.handle.as_raw_handle().cast(), /*uExitCode*/ 1)
        };
        if terminated == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl AsRawHandle for JobObject {
    fn as_raw_handle(&self) -> RawHandle {
        self.handle.as_raw_handle()
    }
}

#[cfg(test)]
#[path = "job_tests.rs"]
mod tests;

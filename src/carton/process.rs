//! The process runtime: the start command runs as a child of the agent in
//! its own process group (a job object on Windows), exactly as tinycd has
//! always run releases. Nothing to install, works everywhere.

use std::path::Path;

use tokio::process::Child;

use super::{CartonSpec, GRACE_PERIOD, log_line, redirect, shell_command};
use crate::util::Error;

pub struct ProcessRuntime;

impl ProcessRuntime {
    /// Link shared paths into the release and run the install command.
    pub async fn build(
        &self,
        release: &Path,
        spec: &CartonSpec,
        log: Option<&std::fs::File>,
    ) -> Result<(), Error> {
        if !spec.share.is_empty() {
            link_shared(&spec.share, &spec.shared_dir, release)?;
        }
        if let Some(install) = &spec.install {
            log_line(log, &format!("running install: {install}"));
            crate::util::say(format!("running install: {install}"));
            let mut process = shell_command(&spec.shell, &spec.env, install, release);
            redirect(&mut process, log)?;
            let status = process.status().await?;
            if !status.success() {
                return Err(format!("install command exited with {status}").into());
            }
        }
        Ok(())
    }

    pub fn start(
        &self,
        release: &Path,
        spec: &CartonSpec,
        log: Option<&std::fs::File>,
    ) -> Result<Server, Error> {
        let mut command = shell_command(&spec.shell, &spec.env, &spec.start, release);
        redirect(&mut command, log)?;
        // A dedicated process group lets stop() signal every process the
        // start command spawns, not just the shell, and keeps the terminal's
        // Ctrl+C for tinycd's own shutdown handling.
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);

        #[allow(unused_mut)]
        let mut child = command.spawn()?;
        #[cfg(unix)]
        let group = child.id();
        #[cfg(windows)]
        let job = match assign_job(&child) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.start_kill();
                return Err(error);
            }
        };
        Ok(Server {
            child,
            #[cfg(unix)]
            group,
            #[cfg(windows)]
            job,
        })
    }
}

/// A running release. On Windows a job object tracks every process the
/// start command spawns; closing the handle kills whatever is left in the
/// job, so nothing outlives tinycd even if tinycd crashes.
pub struct Server {
    child: Child,
    #[cfg(unix)]
    group: Option<u32>,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

impl Server {
    pub async fn wait(&mut self) -> Result<String, Error> {
        let status = self.wait_status().await?;
        Ok(format!("the server exited with {status}"))
    }

    pub async fn wait_status(&mut self) -> Result<std::process::ExitStatus, Error> {
        Ok(self.child.wait().await?)
    }

    pub fn exited(&mut self) -> Result<Option<String>, Error> {
        Ok(self
            .child
            .try_wait()?
            .map(|status| format!("the server exited with {status}")))
    }

    pub fn detach(mut self) -> Result<(), Error> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::{
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            };
            let limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe {
                SetInformationJobObject(
                    self.job.as_raw_handle() as _,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&limits).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        #[cfg(unix)]
        {
            self.group = None;
        }
        Ok(())
    }

    /// Stop the whole process tree: ask politely, wait up to GRACE_PERIOD,
    /// then kill. Politely means SIGTERM to the process group on Unix and
    /// Ctrl+Break on Windows, which only reaches the server when it shares
    /// tinycd's console.
    pub async fn stop(mut self) -> Result<(), Error> {
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            unsafe { libc::killpg(group as i32, libc::SIGTERM) };
            if tokio::time::timeout(GRACE_PERIOD, self.child.wait())
                .await
                .is_err()
            {
                crate::util::say_err(format!(
                    "server did not exit within {} seconds, killing it",
                    GRACE_PERIOD.as_secs()
                ));
            }
            unsafe { libc::killpg(group as i32, libc::SIGKILL) };
        }

        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;

            let polite = self.child.id().is_some_and(|group| unsafe {
                GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, group) != 0
            });
            if polite
                && tokio::time::timeout(GRACE_PERIOD, self.child.wait())
                    .await
                    .is_err()
            {
                crate::util::say_err(format!(
                    "server did not exit within {} seconds, killing it",
                    GRACE_PERIOD.as_secs()
                ));
            }
            unsafe { TerminateJobObject(self.job.as_raw_handle() as _, 1) };
        }

        self.child
            .wait()
            .await
            .map_err(|error| format!("failed to stop the running server: {error}"))?;
        Ok(())
    }
}

// A shell can exit before its descendants. Keep the process group until
// the handle is released, including failed verification and cancelled tasks.
#[cfg(unix)]
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(group) = self.group.take() {
            unsafe { libc::killpg(group as i32, libc::SIGKILL) };
        }
    }
}

/// Put the spawned server into a new job object that kills every remaining
/// process in the job when the handle closes.
#[cfg(windows)]
fn assign_job(child: &Child) -> Result<std::os::windows::io::OwnedHandle, Error> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };

    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            raw,
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }

    let process = child
        .raw_handle()
        .ok_or("the server exited before it could be tracked")?;
    if unsafe { AssignProcessToJobObject(raw, process) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(job)
}

/// Symlink each shared entry from <shared> into the release, replacing
/// whatever the sync produced at that path. Missing sources are created:
/// entries with a trailing slash as directories, others as empty files. On
/// Windows directories become junctions and files hard links, neither of
/// which needs administrator rights.
pub fn link_shared(entries: &[String], shared: &Path, release: &Path) -> Result<(), Error> {
    for entry in entries {
        let relative = entry.trim_end_matches(['/', '\\']);
        let source = shared.join(relative);
        let target = release.join(relative);

        if !source.exists() {
            if entry.ends_with(['/', '\\']) {
                std::fs::create_dir_all(&source)?;
            } else {
                if let Some(parent) = source.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::File::create(&source)?;
                crate::util::say(format!("created empty shared file {}", source.display()));
            }
        }

        if let Ok(existing) = std::fs::symlink_metadata(&target) {
            if existing.is_dir() {
                std::fs::remove_dir_all(&target)?;
            } else {
                std::fs::remove_file(&target)?;
            }
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let source = std::path::absolute(&source)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, &target)
            .map_err(|error| format!("failed to link {}: {error}", target.display()))?;
        #[cfg(windows)]
        if source.is_dir() {
            junction::create(&source, &target)
                .map_err(|error| format!("failed to link {}: {error}", target.display()))?;
        } else {
            std::fs::hard_link(&source, &target)
                .map_err(|error| format!("failed to link {}: {error}", target.display()))?;
        }
        #[cfg(not(any(unix, windows)))]
        return Err("share is not supported on this platform".into());
    }
    Ok(())
}

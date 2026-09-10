//! Own only this MCP server's process tree. This is lifecycle containment,
//! not a filesystem/network sandbox or a boundary against hostile same-user code.
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::process::{Child, Command};

pub(super) struct ProcessOwner {
    stopped: AtomicBool,
    #[cfg(windows)]
    job: super::windows_job::KitJob,
    #[cfg(unix)]
    group: i32,
}

impl ProcessOwner {
    pub fn spawn(command: &mut Command) -> Result<(Child, Self)> {
        #[cfg(windows)]
        {
            let (child, job) = super::windows_job::KitJob::spawn(command)?;
            Ok((
                child,
                Self {
                    stopped: AtomicBool::new(false),
                    job,
                },
            ))
        }
        #[cfg(unix)]
        {
            // A dedicated group keeps ordinary launcher descendants together.
            command.process_group(0);
            let child = command.spawn()?;
            let group = child
                .id()
                .ok_or_else(|| anyhow::anyhow!("MCP process has no PID"))?
                as i32;
            Ok((
                child,
                Self {
                    stopped: AtomicBool::new(false),
                    group,
                },
            ))
        }
        #[cfg(not(any(windows, unix)))]
        anyhow::bail!("MCP process ownership is unsupported on this platform")
    }

    pub fn terminate(&self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        #[cfg(windows)]
        self.job.terminate();
        #[cfg(unix)]
        unsafe {
            libc::kill(-self.group, libc::SIGKILL);
        }
    }
}

impl Drop for ProcessOwner {
    fn drop(&mut self) {
        self.terminate();
    }
}

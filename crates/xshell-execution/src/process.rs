use tokio::process::{Child, Command};

pub(crate) fn configure_process_group(command: &mut Command) {
    command.process_group(0).kill_on_drop(true);
}

pub(crate) struct ProcessGroupGuard {
    process_group: Option<i32>,
    armed: bool,
}

impl ProcessGroupGuard {
    pub(crate) fn for_child(child: &Child) -> Self {
        Self {
            process_group: child.id().and_then(|pid| i32::try_from(pid).ok()),
            armed: true,
        }
    }

    pub(crate) fn kill(&self) {
        if self.armed
            && let Some(process_group) = self
                .process_group
                .filter(|process_group| *process_group > 1)
        {
            // SAFETY: kill(2) with a negative pid targets a process group. It
            // has no memory-safety preconditions; ESRCH means it already exited.
            unsafe {
                libc::kill(-process_group, libc::SIGKILL);
            }
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

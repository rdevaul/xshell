use std::time::Duration;
use tokio::process::{Child, Command};

/// Extra group-wide `SIGKILL`s sent by [`ProcessGroupGuard::sweep`], and the
/// pause before each. Together they cover the window in which a fork that
/// overlapped the first kill can complete.
const SWEEP_ROUNDS: usize = 6;
const SWEEP_INTERVAL: Duration = Duration::from_millis(5);

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

    /// Send `signal` to every member of the group. Returns whether the group
    /// still had members to receive it.
    fn signal(&self, signal: libc::c_int) -> bool {
        let Some(process_group) = self
            .process_group
            .filter(|process_group| self.armed && *process_group > 1)
        else {
            return false;
        };
        // SAFETY: kill(2) with a negative pid targets a process group. It
        // has no memory-safety preconditions; ESRCH means the group is empty.
        unsafe { libc::kill(-process_group, signal) == 0 }
    }

    pub(crate) fn kill(&self) {
        self.signal(libc::SIGKILL);
    }

    /// Kill the group again until it is empty, a bounded number of times.
    /// Call after [`kill`](Self::kill) when the outcome is reported as the
    /// group having been killed.
    ///
    /// One group-wide `SIGKILL` is not enough. A process whose fork overlaps
    /// the signal can join the group just after delivery and keep running:
    /// Linux aborts such a fork, but macOS does not guarantee it. Every later
    /// round reaches that straggler.
    ///
    /// Sweeping stops at the first round that finds the group empty. While a
    /// round succeeds the group still exists, so its ID cannot have been
    /// reused by an unrelated group; after the group empties, it is never
    /// signalled again.
    pub(crate) async fn sweep(&self) {
        for _ in 0..SWEEP_ROUNDS {
            tokio::time::sleep(SWEEP_INTERVAL).await;
            if !self.signal(libc::SIGKILL) {
                return;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    fn sleeper() -> Command {
        let mut command = Command::new("sleep");
        command
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command
    }

    /// Joining a group whose only member is an unreaped leader relies on
    /// Linux keeping the zombie in its group until it is reaped.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sweep_kills_a_member_that_joined_after_the_first_kill() {
        let mut leader_command = sleeper();
        leader_command.process_group(0);
        let mut leader = leader_command.spawn().unwrap();
        let group = ProcessGroupGuard::for_child(&leader);
        let process_group = group.process_group.unwrap();

        group.kill();
        // The leader is now an unreaped zombie, which keeps the group alive.
        // A process created now stands in for one whose fork overlapped the
        // kill: the single signal has already been delivered and missed it.
        let mut straggler_command = sleeper();
        straggler_command.process_group(process_group);
        let mut straggler = straggler_command.spawn().unwrap();

        group.sweep().await;
        let status = tokio::time::timeout(Duration::from_secs(5), straggler.wait())
            .await
            .expect("the straggler survived the sweep")
            .unwrap();
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(libc::SIGKILL)
        );
        leader.wait().await.unwrap();
    }

    #[tokio::test]
    async fn sweep_stops_once_the_group_is_empty() {
        let mut leader_command = sleeper();
        leader_command.process_group(0);
        let mut leader = leader_command.spawn().unwrap();
        let group = ProcessGroupGuard::for_child(&leader);
        group.kill();
        leader.wait().await.unwrap();

        // Reaped and empty: the first round finds nothing and returns at once
        // rather than signalling a group ID that could be reused.
        let started = std::time::Instant::now();
        group.sweep().await;
        assert!(started.elapsed() < SWEEP_INTERVAL * 3);
    }

    #[tokio::test]
    async fn a_disarmed_guard_signals_nothing() {
        let mut leader_command = sleeper();
        leader_command.process_group(0);
        let mut leader = leader_command.spawn().unwrap();
        let mut group = ProcessGroupGuard::for_child(&leader);
        group.disarm();
        group.kill();
        group.sweep().await;
        assert!(
            leader.try_wait().unwrap().is_none(),
            "a disarmed guard killed"
        );
        leader.kill().await.unwrap();
    }
}

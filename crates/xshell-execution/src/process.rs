use crate::engine::CancellationFlag;
use anyhow::{Context, Result};
use std::process::ExitStatus;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::time::Instant;

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

/// The default for how long a shell's output is still drained after the
/// shell itself exits (see [`supervise_shell`]'s `linger_grace`). Past it,
/// any process still holding the output pipes (typically a background job
/// started with `&`) is killed with the rest of the group.
pub(crate) const LINGER_GRACE: Duration = Duration::from_secs(2);
/// After a group kill, how long output already written to the pipes is still
/// collected before the readers are abandoned.
const KILLED_DRAIN: Duration = Duration::from_millis(250);
/// After a group kill, how long the shell leader is waited for. A `SIGKILL`ed
/// leader is reaped at once; this only bounds a pathological kernel state.
const KILLED_REAP: Duration = Duration::from_secs(2);

/// Which pipe a chunk of shell output arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

/// How a supervised shell ended.
#[derive(Debug)]
pub(crate) enum ShellEnd {
    /// The shell exited and its output pipes closed within the linger grace.
    Exited(ExitStatus),
    /// The shell exited, but something still held its output pipes after
    /// the linger grace (or at the deadline, if that came first). The process
    /// group was killed; the output collected until then is kept.
    ExitedLingering(ExitStatus),
    /// Cancelled before the output was drained; the process group was killed.
    Cancelled,
    /// The deadline passed while the shell was running; the process group
    /// was killed.
    TimedOut,
}

/// Run a spawned shell (placed in its own group by
/// [`configure_process_group`], with piped stdout and stderr) to completion,
/// passing each chunk of output to `on_output`.
///
/// Every wait, including draining the pipes after the shell exits, sits
/// inside one `select!` with cancellation and the deadline, so a background
/// job that inherits the pipes cannot hold the caller past `timeout` plus a
/// short, fixed kill-and-drain allowance (well under a few seconds). When the
/// call ends by cancellation, timeout, or a lingering pipe holder, the whole
/// group is killed and swept, and the reader tasks are aborted rather than
/// awaited to EOF. The group stays armed until then, so dropping this future
/// early also kills the group.
///
/// Limitation: a process that leaves the group (`setsid`, or `setpgid` to
/// another group) cannot be reached by the group kill. It is never waited
/// for (its readers are abandoned, so it gets `SIGPIPE` on its next write),
/// but it can keep running. Containing it needs an OS-level sandbox.
///
/// A background job that redirects its output away from the pipes (for
/// example `server > log 2>&1 &`) does not hold them, so after a normal exit
/// it is left running, as in an ordinary shell.
///
/// `linger_grace` is how long the pipes may stay open after the shell exits
/// ([`LINGER_GRACE`] in production); it is capped by the remaining timeout.
pub(crate) async fn supervise_shell(
    mut child: Child,
    cancellation: &CancellationFlag,
    timeout: Duration,
    linger_grace: Duration,
    mut on_output: impl FnMut(Stream, &[u8]) + Send,
) -> Result<ShellEnd> {
    let deadline = Instant::now() + timeout;
    let mut group = ProcessGroupGuard::for_child(&child);
    let stdout = child.stdout.take().context("cannot capture shell stdout")?;
    let stderr = child.stderr.take().context("cannot capture shell stderr")?;
    let (sender, mut receiver) = mpsc::channel::<(Stream, Vec<u8>)>(32);
    let stdout_task = tokio::spawn(read_output(Stream::Stdout, stdout, sender.clone()));
    let stderr_task = tokio::spawn(read_output(Stream::Stderr, stderr, sender));
    let mut wait_task = tokio::spawn(async move { child.wait().await });

    let mut status: Option<ExitStatus> = None;
    let mut linger_until = deadline;
    let mut pipes_open = true;
    let stop = loop {
        if !pipes_open && let Some(status) = status {
            // Both readers have returned, so these joins complete at once.
            stdout_task.await.context("shell stdout reader failed")??;
            stderr_task.await.context("shell stderr reader failed")??;
            // The leader is reaped and nothing holds the pipes. Members that
            // redirected their output may remain; they are left alone.
            group.disarm();
            return Ok(ShellEnd::Exited(status));
        }
        tokio::select! {
            item = receiver.recv(), if pipes_open => match item {
                Some((stream, bytes)) => on_output(stream, &bytes),
                None => pipes_open = false,
            },
            result = &mut wait_task, if status.is_none() => {
                let exited = result
                    .context("shell wait task failed")?
                    .context("could not wait for shell")?;
                status = Some(exited);
                linger_until = deadline.min(Instant::now() + linger_grace);
            }
            () = cancellation.wait() => break ShellEnd::Cancelled,
            () = tokio::time::sleep_until(linger_until) => {
                break match status {
                    Some(status) => ShellEnd::ExitedLingering(status),
                    None => ShellEnd::TimedOut,
                };
            }
        }
    };

    group.kill();
    group.sweep().await;
    if status.is_none() {
        let _ = tokio::time::timeout(KILLED_REAP, &mut wait_task).await;
    }
    // Keep what the killed processes had already written, but do not wait for
    // EOF: a process outside the group may hold the pipes indefinitely.
    let drain_until = Instant::now() + KILLED_DRAIN;
    while pipes_open {
        tokio::select! {
            item = receiver.recv() => match item {
                Some((stream, bytes)) => on_output(stream, &bytes),
                None => pipes_open = false,
            },
            () = tokio::time::sleep_until(drain_until) => break,
        }
    }
    stdout_task.abort();
    stderr_task.abort();
    wait_task.abort();
    // The sweep has run; never signal this group ID again.
    group.disarm();
    Ok(stop)
}

/// Forward a pipe to `sender` until EOF. Stops quietly if the receiver is
/// gone, so an abandoned reader never blocks on a full channel.
async fn read_output(
    stream: Stream,
    mut reader: impl AsyncRead + Unpin,
    sender: mpsc::Sender<(Stream, Vec<u8>)>,
) -> std::io::Result<()> {
    let mut buffer = vec![0_u8; 8 * 1024];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        if sender
            .send((stream, buffer[..count].to_vec()))
            .await
            .is_err()
        {
            return Ok(());
        }
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

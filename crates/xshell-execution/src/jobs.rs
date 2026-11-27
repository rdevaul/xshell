//! Background shell jobs: `run_shell` with `background: true`, and the
//! processes a foreground command leaves running.
//!
//! A job is a process group xshell started. xshell holds the group outside
//! any sandbox that contains the commands, so it can always stop it, which
//! the commands themselves may not be able to do: under the macOS profile
//! each command is its own sandbox, and signals cross no sandbox boundary.
//!
//! - **Background jobs** are started by `run_shell` with `background: true`.
//!   Their combined output (stdout and stderr, in arrival order) is kept in a
//!   bounded buffer and read with `job_output`, which can also wait for the
//!   job to finish. `job_kill` kills the whole group.
//! - **Leftovers.** A foreground command can start processes that outlive it
//!   (`build > log 2>&1 &` holds none of the command's pipes, so it is not
//!   killed when the command returns). When a command's group still has
//!   members after it exits, the group is adopted as a job, so the agent is
//!   told about it and can stop it. Its output went elsewhere, so a leftover
//!   job has none to show.
//! - **Notices.** When a job ends, the next tool result says so, once.
//! - **Lifetime.** Jobs belong to the root turn: they are shared with its
//!   subagents, and every group still running is killed when the turn's
//!   budget is dropped, on every way a turn can end.
//!
//! A leftover group is watched once a second until it empties. Its ID is
//! never signalled after that, which leaves only the window between a check
//! and the group's ID being reused, about a second after it emptied and only
//! if the system wraps its process IDs that fast.

use crate::engine::CancellationFlag;
use crate::process::{LINGER_GRACE, ShellEnd, supervise_shell};
use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::Child;
use tokio::sync::Notify;
use tokio::time::Instant;

/// Output kept per job; older output is dropped first.
pub(crate) const JOB_OUTPUT_BYTES: usize = 1 << 20;
/// Most output one `job_output` call returns; the newest is shown.
pub(crate) const JOB_READ_BYTES: usize = 64 * 1024;
/// Jobs that may run at once.
pub(crate) const MAX_RUNNING_JOBS: usize = 8;
/// Leftover groups watched at once. A command that leaves more running has
/// its group killed at once instead.
pub(crate) const MAX_LEFTOVERS: usize = 16;
/// Output kept, in all, for jobs that have ended; the oldest ended job's
/// output is freed first.
const ENDED_OUTPUT_BYTES: usize = 8 << 20;
/// Longest single wait `job_output` accepts.
pub(crate) const MAX_JOB_WAIT: Duration = Duration::from_secs(1800);
/// How often a leftover group is checked for members.
const LEFTOVER_POLL: Duration = Duration::from_secs(1);
/// A background job has no time limit of its own; the turn bounds it.
const NO_TIMEOUT: Duration = Duration::from_secs(10 * 365 * 24 * 3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Background,
    Leftover,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum End {
    /// The shell exited with this status (as text, e.g. "0" or "signal 9").
    Exited(String),
    /// The shell exited, but processes still held its output and were killed.
    ExitedLingering(String),
    Killed,
    /// A leftover group whose members have all exited.
    Emptied,
}

#[derive(Debug)]
struct Job {
    command: String,
    kind: Kind,
    started: Instant,
    group: i32,
    output: VecDeque<u8>,
    /// Bytes of output received in all, and how many of them were shown.
    received: u64,
    shown: u64,
    end: Option<(End, Instant)>,
    /// Whether the agent has been told the job ended.
    reported: bool,
    stop: CancellationFlag,
}

impl Job {
    fn running(&self) -> bool {
        self.end.is_none()
    }

    fn status(&self) -> String {
        let label = match self.kind {
            Kind::Background => "background job",
            Kind::Leftover => "leftover processes",
        };
        match &self.end {
            None => format!("{label}, running for {}", elapsed(self.started.elapsed())),
            Some((end, at)) => {
                let lasted = elapsed(at.duration_since(self.started));
                match end {
                    End::Exited(status) => {
                        format!("{label}, exited with status {status} after {lasted}")
                    }
                    End::ExitedLingering(status) => format!(
                        "{label}, exited with status {status} after {lasted}; processes still \
holding its output were killed"
                    ),
                    End::Killed => format!("{label}, killed after {lasted}"),
                    End::Emptied => format!("{label}, all exited within {lasted}"),
                }
            }
        }
    }
}

/// The jobs of one root turn. See the module documentation.
#[derive(Debug, Default)]
pub struct ShellJobs {
    state: Mutex<State>,
    changed: Notify,
}

#[derive(Debug, Default)]
struct State {
    next_id: u32,
    jobs: BTreeMap<u32, Job>,
    shut_down: bool,
    /// The turn's deadline; no `job_output` wait outlasts it.
    deadline: Option<Instant>,
}

impl ShellJobs {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The jobs of a turn that ends at `deadline`.
    pub fn until(deadline: Instant) -> Arc<Self> {
        let jobs = Self::new();
        jobs.lock().deadline = Some(deadline);
        jobs
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Whether another background job may start, or why not.
    pub(crate) fn can_start(&self) -> Result<(), String> {
        let state = self.lock();
        if state.shut_down {
            return Err("the turn has ended".into());
        }
        let running = state
            .jobs
            .values()
            .filter(|job| job.running() && job.kind == Kind::Background)
            .count();
        if running >= MAX_RUNNING_JOBS {
            return Err(format!(
                "{running} background jobs are already running, the most allowed; wait for one \
with job_output or stop one with job_kill"
            ));
        }
        Ok(())
    }

    /// Supervise `child`, a shell started for a background job, and return
    /// the job's ID. The child must have piped stdout and stderr and lead
    /// its own process group.
    pub(crate) fn start(self: &Arc<Self>, command: &str, child: Child) -> u32 {
        let group = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .unwrap_or(0);
        let stop = CancellationFlag::default();
        let id = self.insert(command, Kind::Background, group, stop.clone());
        let jobs = Arc::clone(self);
        tokio::spawn(async move {
            let output = Arc::clone(&jobs);
            let end = supervise_shell(child, &stop, NO_TIMEOUT, LINGER_GRACE, move |_, bytes| {
                output.append(id, bytes);
            })
            .await;
            let end = match end {
                Ok(ShellEnd::Exited(status)) => {
                    // The shell exited, but something it started in the
                    // background (`server >log 2>&1 &`) may still run in its
                    // group, holding none of its output. Watch that as a
                    // leftover, so it is reported and killed with the turn.
                    if group_alive(group) {
                        let command = jobs.lock().jobs.get(&id).map(|job| job.command.clone());
                        if let Some(command) = command {
                            jobs.adopt(&command, group);
                        }
                    }
                    End::Exited(status_text(status))
                }
                Ok(ShellEnd::ExitedLingering(status)) => End::ExitedLingering(status_text(status)),
                Ok(ShellEnd::Cancelled | ShellEnd::TimedOut) => End::Killed,
                Err(error) => End::Exited(format!("unknown ({error:#})")),
            };
            jobs.finish(id, end);
        });
        id
    }

    /// Adopt `group`, left running by the foreground command `command`, as
    /// a job. Returns its ID, or `None` once the turn has ended (the group is
    /// then killed at once).
    pub(crate) fn adopt(self: &Arc<Self>, command: &str, group: i32) -> Option<u32> {
        if group <= 1 {
            return None;
        }
        {
            let state = self.lock();
            let leftovers = state
                .jobs
                .values()
                .filter(|job| job.running() && job.kind == Kind::Leftover)
                .count();
            if state.shut_down || leftovers >= MAX_LEFTOVERS {
                drop(state);
                kill_group(group);
                return None;
            }
        }
        let id = self.insert(command, Kind::Leftover, group, CancellationFlag::default());
        let jobs = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(LEFTOVER_POLL).await;
                if !jobs.is_running(id) {
                    return;
                }
                if !group_alive(group) {
                    jobs.finish(id, End::Emptied);
                    return;
                }
            }
        });
        Some(id)
    }

    fn insert(&self, command: &str, kind: Kind, group: i32, stop: CancellationFlag) -> u32 {
        let mut state = self.lock();
        state.next_id += 1;
        let id = state.next_id;
        state.jobs.insert(
            id,
            Job {
                command: command.to_owned(),
                kind,
                started: Instant::now(),
                group,
                output: VecDeque::new(),
                received: 0,
                shown: 0,
                end: None,
                reported: false,
                stop,
            },
        );
        id
    }

    fn append(&self, id: u32, bytes: &[u8]) {
        let mut state = self.lock();
        if let Some(job) = state.jobs.get_mut(&id) {
            job.output.extend(bytes);
            let excess = job.output.len().saturating_sub(JOB_OUTPUT_BYTES);
            job.output.drain(..excess);
            job.received += bytes.len() as u64;
        }
    }

    fn finish(&self, id: u32, end: End) {
        {
            let mut state = self.lock();
            if let Some(job) = state.jobs.get_mut(&id)
                && job.end.is_none()
            {
                job.end = Some((end, Instant::now()));
            }
            state.trim_ended_output();
        }
        self.changed.notify_waiters();
    }

    fn is_running(&self, id: u32) -> bool {
        self.lock().jobs.get(&id).is_some_and(Job::running)
    }

    #[cfg(test)]
    pub(crate) fn group_of(&self, id: u32) -> Option<i32> {
        self.lock().jobs.get(&id).map(|job| job.group)
    }

    /// Whether a background job (not a leftover) is still running.
    pub(crate) fn background_running(&self) -> bool {
        self.lock()
            .jobs
            .values()
            .any(|job| job.running() && job.kind == Kind::Background)
    }

    /// `job_output`: the job's status and the output it produced since the
    /// last read, after waiting up to `wait` for it to end.
    pub(crate) async fn output(
        &self,
        id: u32,
        wait: Duration,
        cancellation: &CancellationFlag,
    ) -> Result<String, String> {
        let mut deadline = Instant::now() + wait.min(MAX_JOB_WAIT);
        if let Some(turn_ends) = self.lock().deadline {
            deadline = deadline.min(turn_ends);
        }
        loop {
            let notified = self.changed.notified();
            match self.lock().jobs.get(&id) {
                None => return Err(format!("there is no job {id}")),
                Some(job) if !job.running() => break,
                Some(_) => {}
            }
            tokio::select! {
                () = notified => {}
                () = tokio::time::sleep_until(deadline) => break,
                () = cancellation.wait() => break,
            }
        }
        let mut state = self.lock();
        let job = state.jobs.get_mut(&id).expect("jobs are never removed");
        let mut text = format!("job {id} ({}): {}\n", job.command_summary(), job.status());
        if !job.running() {
            job.reported = true;
        }
        if job.kind == Kind::Leftover {
            text.push_str("its output went to wherever the command redirected it\n");
            return Ok(text);
        }
        let unread = job.received - job.shown;
        if unread == 0 {
            text.push_str("no new output\n");
            return Ok(text);
        }
        let kept = job.output.len() as u64;
        let shown = unread.min(kept).min(JOB_READ_BYTES as u64) as usize;
        let skipped = unread - shown as u64;
        if skipped > 0 {
            let _ = writeln!(text, "[{skipped} earlier bytes of new output not shown]");
        }
        let start = job.output.len() - shown;
        let bytes: Vec<u8> = job.output.range(start..).copied().collect();
        text.push_str(&String::from_utf8_lossy(&bytes));
        if !text.ends_with('\n') {
            text.push('\n');
        }
        job.shown = job.received;
        // An ended job whose output has all been read keeps none.
        if !job.running() {
            job.output = VecDeque::new();
        }
        Ok(text)
    }

    /// `job_kill`: kill the job's process group and wait briefly for the
    /// kill to be recorded.
    pub(crate) async fn kill(&self, id: u32) -> Result<String, String> {
        let (kind, stop, group) = match self.lock().jobs.get(&id) {
            None => return Err(format!("there is no job {id}")),
            Some(job) if !job.running() => {
                return Ok(format!("job {id} had already ended: {}", job.status()));
            }
            Some(job) => (job.kind, job.stop.clone(), job.group),
        };
        match kind {
            Kind::Background => stop.cancel(),
            Kind::Leftover => {
                kill_group(group);
                self.finish(id, End::Killed);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.is_running(id) && Instant::now() < deadline {
            let notified = self.changed.notified();
            tokio::select! {
                () = notified => {}
                () = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
        let mut state = self.lock();
        let job = state.jobs.get_mut(&id).expect("jobs are never removed");
        job.reported = true;
        Ok(format!("job {id}: {}", job.status()))
    }

    /// Whether a job ended that the agent has not been told about.
    pub(crate) fn has_notices(&self) -> bool {
        self.lock()
            .jobs
            .values()
            .any(|job| !job.running() && !job.reported)
    }

    /// One line per job that ended since the agent was last told, marking
    /// them told. Empty when there is nothing new.
    pub(crate) fn take_notices(&self) -> String {
        let mut state = self.lock();
        let mut text = String::new();
        for (id, job) in &mut state.jobs {
            if job.reported || job.running() {
                continue;
            }
            job.reported = true;
            let _ = write!(
                text,
                "\n[xshell] job {id} ({}): {}.",
                job.command_summary(),
                job.status()
            );
            if job.kind == Kind::Background && job.received > job.shown {
                let _ = write!(text, " job_output {id} shows its remaining output.");
            }
        }
        text
    }

    /// The running background jobs, one line each, for the event that says
    /// the turn is waiting on them.
    pub(crate) fn running_summary(&self) -> String {
        let state = self.lock();
        state
            .jobs
            .iter()
            .filter(|(_, job)| job.running() && job.kind == Kind::Background)
            .map(|(id, job)| format!("job {id} ({}): {}", job.command_summary(), job.status()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Wait until a background job ends, cancellation, or `deadline`.
    pub(crate) async fn wait_for_background(
        &self,
        cancellation: &CancellationFlag,
        deadline: Instant,
    ) {
        loop {
            let notified = self.changed.notified();
            if !self.background_running() {
                return;
            }
            let before = self.ended_count();
            tokio::select! {
                () = notified => {
                    if self.ended_count() > before {
                        return;
                    }
                }
                () = tokio::time::sleep_until(deadline) => return,
                () = cancellation.wait() => return,
            }
        }
    }

    fn ended_count(&self) -> usize {
        self.lock()
            .jobs
            .values()
            .filter(|job| !job.running())
            .count()
    }

    /// Kill every running job's group and refuse new jobs. Called when the
    /// turn's budget is dropped; it blocks for a few milliseconds at most.
    pub(crate) fn shut_down(&self) {
        let mut state = self.lock();
        state.shut_down = true;
        for job in state.jobs.values_mut() {
            if job.running() {
                job.stop.cancel();
                kill_group(job.group);
                job.end = Some((End::Killed, Instant::now()));
            }
        }
        drop(state);
        self.changed.notify_waiters();
    }
}

impl State {
    /// Free the output of the oldest ended jobs until what ended jobs keep
    /// fits in [`ENDED_OUTPUT_BYTES`].
    fn trim_ended_output(&mut self) {
        let mut kept: usize = self
            .jobs
            .values()
            .filter(|job| !job.running())
            .map(|job| job.output.len())
            .sum();
        for job in self.jobs.values_mut() {
            if kept <= ENDED_OUTPUT_BYTES {
                break;
            }
            if !job.running() && !job.output.is_empty() {
                kept -= job.output.len();
                job.output = VecDeque::new();
            }
        }
    }
}

impl Job {
    fn command_summary(&self) -> String {
        let line = self.command.lines().next().unwrap_or_default().trim();
        let mut summary: String = line.chars().take(80).collect();
        if summary.len() < self.command.trim().len() {
            summary.push('…');
        }
        format!("`{summary}`")
    }
}

/// Whether the process group still has members.
pub(crate) fn group_alive(group: i32) -> bool {
    // SAFETY: kill(2) with signal 0 only checks; a negative pid names a group.
    group > 1 && unsafe { libc::kill(-group, 0) } == 0
}

/// Kill every member of the group, repeating briefly for members whose fork
/// overlapped the first signal (see `ProcessGroupGuard::sweep`).
fn kill_group(group: i32) {
    if group <= 1 {
        return;
    }
    for round in 0..6 {
        if round > 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        // SAFETY: kill(2) with a negative pid targets a process group;
        // ESRCH means it is empty.
        if unsafe { libc::kill(-group, libc::SIGKILL) } != 0 {
            return;
        }
    }
}

pub(crate) fn status_text(status: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => code.to_string(),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "unknown".into(),
    }
}

fn elapsed(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 120 {
        format!("{seconds} s")
    } else {
        format!("{} min {} s", seconds / 60, seconds % 60)
    }
}

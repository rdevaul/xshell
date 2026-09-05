use crate::{PtyDescriptor, PtySize, PtyTicket, SessionAuditHandle, TerminalStreamPolicy};
use anyhow::{Context, Result, bail};
use base64::Engine;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
use uuid::Uuid;
use xshell_audit::AuditEvent;
use xshell_platform::LockExt;
use xshell_pty::{PtySize as ProcessSize, RemotePtyProcess};

const MAX_ACTIVE_PTYS: usize = 64;
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_INPUT_BYTES: usize = 64 * 1024;
const WORKER_INPUT_BYTES: usize = 16 * 1024;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_STREAM_OUTPUT_BYTES: usize = MAX_OUTPUT_BYTES - 8;
const REPLAY_BYTES: usize = 1024 * 1024;
const WORKER_WAIT: Duration = Duration::from_millis(40);
const MAX_READ_WAIT: Duration = Duration::from_millis(250);
const MAX_DIMENSION: u16 = 1_000;
const MAX_TERMINAL_TYPE_BYTES: usize = 128;
/// Largest `data` payload (pre-base64) in one `TerminalStream` audit record.
/// Keeps each record comfortably inside the audit service's request bound.
const STREAM_RECORD_BYTES: usize = 64 * 1024;

#[derive(Clone, Default)]
pub struct PtyCoordinator {
    inner: Arc<Mutex<HashMap<String, Arc<ManagedPty>>>>,
}

impl Drop for PtyCoordinator {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) != 1 {
            return;
        }
        let managed = self
            .inner
            .lock_recover()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for pty in managed {
            shutdown(&pty);
        }
    }
}

struct ManagedPty {
    session_id: String,
    command: String,
    cwd: String,
    audit: Option<SessionAuditHandle>,
    /// Opt-in byte-stream capture; `None` records lifecycle only.
    capture: Option<Mutex<StreamCapture>>,
    stream: Mutex<StreamAuthorization>,
    state: Mutex<PtyState>,
    changed: Condvar,
}

/// How a terminal job is audited: lifecycle through `handle`, and optionally
/// the byte stream as well.
#[derive(Clone)]
pub struct PtyAudit {
    pub handle: SessionAuditHandle,
    /// `Some` enables byte-for-byte stream capture under the given policy.
    pub stream: Option<TerminalStreamPolicy>,
}

/// Bookkeeping for byte-for-byte terminal-stream auditing of one job.
/// Only bytes the process actually accepted (input) or produced (output) are
/// recorded, so the trail reflects what happened rather than what was queued.
struct StreamCapture {
    budget: Option<u64>,
    recorded: u64,
    dropped: u64,
    input_offset: u64,
    output_offset: u64,
    failed: bool,
}

impl StreamCapture {
    fn new(policy: TerminalStreamPolicy) -> Self {
        Self {
            budget: policy.max_bytes,
            recorded: 0,
            dropped: 0,
            input_offset: 0,
            output_offset: 0,
            failed: false,
        }
    }

    /// Split `bytes` into the prefix that fits the remaining budget and count
    /// the rest as dropped. Advances the direction offset by the full length so
    /// offsets stay faithful to the real stream even when bytes are dropped.
    fn take(&mut self, direction: &str, bytes: &[u8]) -> (u64, usize) {
        let offset = match direction {
            "input" => &mut self.input_offset,
            _ => &mut self.output_offset,
        };
        let start = *offset;
        *offset += bytes.len() as u64;
        let allowed = match self.budget {
            None => bytes.len(),
            Some(budget) => {
                let remaining = budget.saturating_sub(self.recorded);
                bytes.len().min(remaining as usize)
            }
        };
        self.recorded += allowed as u64;
        self.dropped += (bytes.len() - allowed) as u64;
        (start, allowed)
    }
}

#[derive(Default)]
struct StreamAuthorization {
    tickets: HashMap<String, u64>,
    claimed: Option<String>,
}

struct PtyState {
    input: VecDeque<u8>,
    size: PtySize,
    output: VecDeque<u8>,
    replay_start: u64,
    replay_end: u64,
    exit_status: Option<String>,
    shutdown: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyClaim {
    pub pty_id: String,
    claim_id: String,
    pub cursor: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyReadResult {
    pub offset: u64,
    pub output: Vec<u8>,
    pub status: Option<String>,
}

impl PtyCoordinator {
    pub fn start(
        &self,
        session_id: &str,
        command: String,
        cwd: &Path,
        size: PtySize,
        terminal_type: Option<String>,
    ) -> Result<PtyTicket> {
        self.start_inner(session_id, command, cwd, size, terminal_type, None)
    }

    /// Start a job whose lifecycle is audited through `audit.handle`. When
    /// `audit.stream` is `Some`, the job's byte stream is also recorded,
    /// bounded by that policy.
    pub fn start_audited(
        &self,
        session_id: &str,
        command: String,
        cwd: &Path,
        size: PtySize,
        terminal_type: Option<String>,
        audit: PtyAudit,
    ) -> Result<PtyTicket> {
        self.start_inner(session_id, command, cwd, size, terminal_type, Some(audit))
    }

    fn start_inner(
        &self,
        session_id: &str,
        command: String,
        cwd: &Path,
        size: PtySize,
        terminal_type: Option<String>,
        audit: Option<PtyAudit>,
    ) -> Result<PtyTicket> {
        validate_command(&command)?;
        validate_size(size)?;
        validate_terminal_type(terminal_type.as_deref())?;
        self.remove_completed_for_session(session_id);
        {
            let ptys = self.inner.lock_recover();
            if ptys.len() >= MAX_ACTIVE_PTYS {
                bail!("PTY capacity is exhausted");
            }
            if ptys.values().any(|pty| pty.session_id == session_id) {
                bail!("session already has a terminal job");
            }
        }
        let (audit, stream) = match audit {
            Some(PtyAudit { handle, stream }) => (Some(handle), stream),
            None => (None, None),
        };
        if let Some(audit) = &audit {
            // This is the final boundary before process creation. In required
            // mode a failed append returns here and the command never starts.
            audit.append(AuditEvent::Input {
                route: "shell_terminal".into(),
                text: format!("${command}"),
            })?;
        }

        let process = RemotePtyProcess::spawn(
            &command,
            cwd,
            ProcessSize {
                rows: size.rows,
                columns: size.columns,
            },
            terminal_type.as_deref(),
        )?;
        let pty_id = Uuid::new_v4().to_string();
        let ticket = Uuid::new_v4().to_string();
        let capture = match (&audit, stream) {
            (Some(_), Some(policy)) => Some(Mutex::new(StreamCapture::new(policy))),
            _ => None,
        };
        let managed = Arc::new(ManagedPty {
            session_id: session_id.to_owned(),
            command,
            cwd: cwd.display().to_string(),
            audit,
            capture,
            stream: Mutex::new(StreamAuthorization {
                tickets: HashMap::from([(ticket.clone(), 0)]),
                claimed: None,
            }),
            state: Mutex::new(PtyState {
                input: VecDeque::new(),
                size,
                output: VecDeque::new(),
                replay_start: 0,
                replay_end: 0,
                exit_status: None,
                shutdown: false,
            }),
            changed: Condvar::new(),
        });
        {
            let mut ptys = self.inner.lock_recover();
            if ptys.len() >= MAX_ACTIVE_PTYS
                || ptys.values().any(|pty| pty.session_id == session_id)
            {
                bail!("session acquired another terminal job while this one was starting");
            }
            ptys.insert(pty_id.clone(), Arc::clone(&managed));
        }
        spawn_worker(managed, process);
        Ok(PtyTicket {
            pty_id,
            ticket,
            replay_from: 0,
        })
    }

    pub fn list(&self) -> Vec<PtyDescriptor> {
        self.inner
            .lock_recover()
            .iter()
            .map(|(pty_id, managed)| descriptor(pty_id, managed))
            .collect()
    }

    pub fn session_id(&self, pty_id: &str) -> Result<String> {
        Ok(self.get(pty_id)?.session_id.clone())
    }

    pub fn attach(&self, session_id: &str, after_offset: Option<u64>) -> Result<PtyTicket> {
        let (pty_id, managed) = self
            .inner
            .lock_recover()
            .iter()
            .find(|(_, pty)| pty.session_id == session_id)
            .map(|(id, pty)| (id.clone(), Arc::clone(pty)))
            .context("session has no terminal job")?;
        let state = managed.state.lock_recover();
        let replay_from = after_offset
            .unwrap_or(state.replay_start)
            .clamp(state.replay_start, state.replay_end);
        drop(state);
        let ticket = Uuid::new_v4().to_string();
        let mut stream = managed.stream.lock_recover();
        if stream.claimed.is_some() {
            bail!("terminal job is already attached");
        }
        stream.tickets.clear();
        stream.tickets.insert(ticket.clone(), replay_from);
        Ok(PtyTicket {
            pty_id,
            ticket,
            replay_from,
        })
    }

    pub fn claim(&self, ticket: &str) -> Result<PtyClaim> {
        let entries = self
            .inner
            .lock_recover()
            .iter()
            .map(|(id, pty)| (id.clone(), Arc::clone(pty)))
            .collect::<Vec<_>>();
        for (pty_id, managed) in entries {
            let mut stream = managed.stream.lock_recover();
            if let Some(cursor) = stream.tickets.remove(ticket) {
                if stream.claimed.is_some() {
                    bail!("terminal job is already attached");
                }
                let claim_id = Uuid::new_v4().to_string();
                stream.tickets.clear();
                stream.claimed = Some(claim_id.clone());
                return Ok(PtyClaim {
                    pty_id,
                    claim_id,
                    cursor,
                });
            }
        }
        bail!("unknown or expired PTY stream ticket")
    }

    pub fn release_claim(&self, claim: &PtyClaim) {
        let Ok(managed) = self.get(&claim.pty_id) else {
            return;
        };
        let mut stream = managed.stream.lock_recover();
        if stream.claimed.as_deref() == Some(claim.claim_id.as_str()) {
            stream.claimed = None;
        }
    }

    pub fn write_claimed(&self, claim: &PtyClaim, bytes: Vec<u8>) -> Result<()> {
        let managed = self.get_claimed(claim)?;
        let mut state = managed.state.lock_recover();
        if state.exit_status.is_some() {
            bail!("terminal job has exited");
        }
        if state.input.len() + bytes.len() > MAX_INPUT_BYTES {
            bail!("terminal job input queue exceeds {MAX_INPUT_BYTES} bytes");
        }
        state.input.extend(bytes);
        Ok(())
    }

    pub fn resize_claimed(&self, claim: &PtyClaim, size: PtySize) -> Result<()> {
        validate_size(size)?;
        let managed = self.get_claimed(claim)?;
        managed.state.lock_recover().size = size;
        Ok(())
    }

    pub fn read_claimed(
        &self,
        claim: &PtyClaim,
        cursor: u64,
        wait_ms: u64,
    ) -> Result<PtyReadResult> {
        let managed = self.get_claimed(claim)?;
        let wait = Duration::from_millis(wait_ms);
        if wait > MAX_READ_WAIT {
            bail!("PTY read wait exceeds {} ms", MAX_READ_WAIT.as_millis());
        }
        let mut state = managed.state.lock_recover();
        let cursor = cursor.max(state.replay_start).min(state.replay_end);
        if cursor == state.replay_end && state.exit_status.is_none() && !state.shutdown {
            let (updated, _) = managed
                .changed
                .wait_timeout(state, wait)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = updated;
        }
        let cursor = cursor.max(state.replay_start).min(state.replay_end);
        let skip = usize::try_from(cursor - state.replay_start).unwrap_or(usize::MAX);
        let output = state
            .output
            .iter()
            .skip(skip)
            .take(MAX_STREAM_OUTPUT_BYTES)
            .copied()
            .collect::<Vec<_>>();
        let delivered_end = cursor.saturating_add(output.len() as u64);
        let status = (delivered_end >= state.replay_end)
            .then(|| state.exit_status.clone())
            .flatten();
        Ok(PtyReadResult {
            offset: cursor,
            output,
            status,
        })
    }

    pub fn terminate(&self, pty_id: &str) -> Result<()> {
        let managed = self
            .inner
            .lock_recover()
            .remove(pty_id)
            .context("unknown terminal job")?;
        shutdown(&managed);
        Ok(())
    }

    pub fn terminate_session(&self, session_id: &str) {
        let removed = {
            let mut ptys = self.inner.lock_recover();
            let ids = ptys
                .iter()
                .filter(|(_, pty)| pty.session_id == session_id)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|id| ptys.remove(&id))
                .collect::<Vec<_>>()
        };
        for managed in &removed {
            shutdown(managed);
        }
    }

    pub fn has_session(&self, session_id: &str) -> bool {
        self.inner.lock_recover().values().any(|pty| {
            pty.session_id == session_id && pty.state.lock_recover().exit_status.is_none()
        })
    }

    fn get(&self, pty_id: &str) -> Result<Arc<ManagedPty>> {
        self.inner
            .lock_recover()
            .get(pty_id)
            .cloned()
            .context("unknown terminal job")
    }

    fn get_claimed(&self, claim: &PtyClaim) -> Result<Arc<ManagedPty>> {
        let managed = self.get(&claim.pty_id)?;
        let claimed =
            managed.stream.lock_recover().claimed.as_deref() == Some(claim.claim_id.as_str());
        if !claimed {
            bail!("PTY stream claim is no longer active");
        }
        Ok(managed)
    }

    fn remove_completed_for_session(&self, session_id: &str) {
        let completed = {
            let ptys = self.inner.lock_recover();
            ptys.iter()
                .find(|(_, pty)| {
                    pty.session_id == session_id && pty.state.lock_recover().exit_status.is_some()
                })
                .map(|(id, _)| id.clone())
        };
        if let Some(id) = completed {
            let _ = self.terminate(&id);
        }
    }
}

fn spawn_worker(managed: Arc<ManagedPty>, mut process: RemotePtyProcess) {
    thread::spawn(move || {
        loop {
            let (input, size, shutdown_requested) = {
                let state = managed.state.lock_recover();
                (
                    state
                        .input
                        .iter()
                        .take(WORKER_INPUT_BYTES)
                        .copied()
                        .collect::<Vec<_>>(),
                    state.size,
                    state.shutdown,
                )
            };
            if shutdown_requested {
                process.terminate();
                finish(&managed, "terminated".into());
                return;
            }
            let result = process.exchange(
                &input,
                ProcessSize {
                    rows: size.rows,
                    columns: size.columns,
                },
                WORKER_WAIT,
                MAX_OUTPUT_BYTES,
            );
            match result {
                Ok(chunk) => {
                    let accepted = chunk.input_accepted.min(input.len());
                    record_stream(&managed, "input", &input[..accepted]);
                    record_stream(&managed, "output", &chunk.output);
                    let mut state = managed.state.lock_recover();
                    for _ in 0..accepted.min(state.input.len()) {
                        state.input.pop_front();
                    }
                    append_output(&mut state, chunk.output);
                    let status = chunk.status;
                    managed.changed.notify_all();
                    drop(state);
                    if let Some(status) = status {
                        finish(&managed, status);
                        return;
                    }
                }
                Err(error) => {
                    finish(&managed, format!("PTY error: {error:#}"));
                    return;
                }
            }
        }
    });
}

fn append_output(state: &mut PtyState, bytes: Vec<u8>) {
    state.replay_end = state.replay_end.saturating_add(bytes.len() as u64);
    state.output.extend(bytes);
    while state.output.len() > REPLAY_BYTES {
        state.output.pop_front();
        state.replay_start = state.replay_start.saturating_add(1);
    }
}

/// Record one direction's bytes for this exchange as `TerminalStream` audit
/// records, honouring the per-job budget. Records are chunked so each stays
/// within the audit service's request bound. Only the bytes the process
/// accepted or produced reach here.
fn record_stream(managed: &ManagedPty, direction: &str, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let (Some(audit), Some(capture)) = (&managed.audit, &managed.capture) else {
        return;
    };
    let mut capture = capture.lock_recover();
    if capture.failed {
        return;
    }
    let (start, allowed) = capture.take(direction, bytes);
    let mut offset = start;
    for chunk in bytes[..allowed].chunks(STREAM_RECORD_BYTES) {
        let event = AuditEvent::TerminalStream {
            command: managed.command.clone(),
            direction: direction.to_owned(),
            offset,
            data: base64::engine::general_purpose::STANDARD.encode(chunk),
            dropped_bytes: 0,
        };
        if let Err(error) = audit.append(event) {
            // Lifecycle records still flow through `audit` (which applies the
            // required/best-effort policy itself); stream capture for this job
            // stops so one failure is not reported once per exchange.
            eprintln!("xshelld audit warning: cannot record terminal stream: {error:#}");
            capture.failed = true;
            return;
        }
        offset += chunk.len() as u64;
    }
}

/// Emit the job's closing stream record when bytes were withheld by the
/// budget, so the trail states what it does not contain.
fn record_stream_summary(managed: &ManagedPty) {
    let (Some(audit), Some(capture)) = (&managed.audit, &managed.capture) else {
        return;
    };
    let (dropped, failed) = {
        let capture = capture.lock_recover();
        (capture.dropped, capture.failed)
    };
    if dropped == 0 || failed {
        return;
    }
    if let Err(error) = audit.append(AuditEvent::TerminalStream {
        command: managed.command.clone(),
        direction: "summary".into(),
        offset: 0,
        data: String::new(),
        dropped_bytes: dropped,
    }) {
        eprintln!("xshelld audit warning: cannot record terminal stream summary: {error:#}");
    }
}

fn finish(managed: &ManagedPty, status: String) {
    let should_audit = {
        let mut state = managed.state.lock_recover();
        let should_audit = state.exit_status.is_none();
        state.exit_status.get_or_insert_with(|| status.clone());
        managed.changed.notify_all();
        should_audit
    };
    if !should_audit {
        return;
    }
    record_stream_summary(managed);
    if let Some(audit) = &managed.audit
        && let Err(error) = audit.append(AuditEvent::ShellFinished {
            command: managed.command.clone(),
            outcome: status,
            cwd: managed.cwd.clone(),
        })
    {
        eprintln!("xshelld audit warning: cannot record terminal completion: {error:#}");
    }
}

fn shutdown(managed: &ManagedPty) {
    {
        managed.state.lock_recover().shutdown = true;
        managed.changed.notify_all();
    }
    // Record termination synchronously so session closure cannot finalize and
    // remove the audit stream before the worker observes the shutdown flag.
    finish(managed, "terminated".into());
}

fn descriptor(pty_id: &str, managed: &ManagedPty) -> PtyDescriptor {
    let stream = managed.stream.lock_recover();
    let state = managed.state.lock_recover();
    PtyDescriptor {
        pty_id: pty_id.to_owned(),
        session_id: managed.session_id.clone(),
        command: managed.command.clone(),
        attached: stream.claimed.is_some(),
        running: state.exit_status.is_none(),
        exit_status: state.exit_status.clone(),
        replay_start: state.replay_start,
        replay_end: state.replay_end,
    }
}

fn validate_command(command: &str) -> Result<()> {
    if command.trim().is_empty() {
        bail!("PTY command is empty");
    }
    if command.len() > MAX_COMMAND_BYTES {
        bail!("PTY command exceeds {MAX_COMMAND_BYTES} bytes");
    }
    Ok(())
}

fn validate_size(size: PtySize) -> Result<()> {
    if size.rows == 0
        || size.columns == 0
        || size.rows > MAX_DIMENSION
        || size.columns > MAX_DIMENSION
    {
        bail!("PTY dimensions must be between 1 and {MAX_DIMENSION}");
    }
    Ok(())
}

fn validate_terminal_type(terminal_type: Option<&str>) -> Result<()> {
    let Some(terminal_type) = terminal_type else {
        return Ok(());
    };
    if terminal_type.is_empty()
        || terminal_type.len() > MAX_TERMINAL_TYPE_BYTES
        || !terminal_type
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.+:".contains(&byte))
    {
        bail!("invalid terminal type");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn terminal_job_survives_stream_detach_and_replays_output() {
        let temporary = TempDir::new().unwrap();
        let coordinator = PtyCoordinator::default();
        let size = PtySize {
            rows: 24,
            columns: 80,
        };
        let ticket = coordinator
            .start(
                "session-a",
                "read value; printf 'remote:%s' \"$value\"; sleep 1".into(),
                temporary.path(),
                size,
                Some("xterm-256color".into()),
            )
            .unwrap();
        let claim = coordinator.claim(&ticket.ticket).unwrap();
        coordinator
            .write_claimed(&claim, b"hello\n".to_vec())
            .unwrap();
        coordinator.release_claim(&claim);
        assert!(coordinator.has_session("session-a"));

        let ticket = coordinator.attach("session-a", Some(0)).unwrap();
        let claim = coordinator.claim(&ticket.ticket).unwrap();
        let mut cursor = claim.cursor;
        let mut output = Vec::new();
        for _ in 0..50 {
            let result = coordinator.read_claimed(&claim, cursor, 100).unwrap();
            cursor = result.offset + result.output.len() as u64;
            output.extend(result.output);
            if result.status.is_some() {
                break;
            }
        }
        assert!(String::from_utf8_lossy(&output).contains("remote:hello"));
    }

    #[test]
    fn stream_capture_budget_drops_bytes_but_keeps_offsets_faithful() {
        let mut capture = StreamCapture::new(TerminalStreamPolicy {
            max_bytes: Some(10),
        });
        assert_eq!(capture.take("input", b"abcd"), (0, 4));
        assert_eq!(capture.take("output", b"12345678"), (0, 6));
        assert_eq!(capture.dropped, 2);
        // Budget is exhausted: nothing more is recorded, offsets still advance.
        assert_eq!(capture.take("output", b"xyz"), (8, 0));
        assert_eq!(capture.take("input", b"e"), (4, 0));
        assert_eq!(capture.dropped, 6);
        assert_eq!(capture.recorded, 10);
    }

    #[test]
    fn stream_capture_without_budget_records_everything() {
        let mut capture = StreamCapture::new(TerminalStreamPolicy { max_bytes: None });
        let large = vec![0u8; 3 * STREAM_RECORD_BYTES + 1];
        assert_eq!(capture.take("output", &large), (0, large.len()));
        assert_eq!(capture.dropped, 0);
    }

    #[test]
    fn validates_remote_pty_bounds() {
        assert!(
            validate_size(PtySize {
                rows: 0,
                columns: 80
            })
            .is_err()
        );
        assert!(validate_terminal_type(Some("xterm;bad")).is_err());
        assert!(validate_command("").is_err());
    }
}

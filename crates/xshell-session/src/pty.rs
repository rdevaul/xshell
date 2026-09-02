use crate::{PtyDescriptor, PtySize, PtyTicket};
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
use uuid::Uuid;
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
            .lock()
            .expect("PTY map poisoned")
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
    stream: Mutex<StreamAuthorization>,
    state: Mutex<PtyState>,
    changed: Condvar,
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
        validate_command(&command)?;
        validate_size(size)?;
        validate_terminal_type(terminal_type.as_deref())?;
        self.remove_completed_for_session(session_id);
        {
            let ptys = self.inner.lock().expect("PTY map poisoned");
            if ptys.len() >= MAX_ACTIVE_PTYS {
                bail!("PTY capacity is exhausted");
            }
            if ptys.values().any(|pty| pty.session_id == session_id) {
                bail!("session already has a terminal job");
            }
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
        let managed = Arc::new(ManagedPty {
            session_id: session_id.to_owned(),
            command,
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
            let mut ptys = self.inner.lock().expect("PTY map poisoned");
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
            .lock()
            .expect("PTY map poisoned")
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
            .lock()
            .expect("PTY map poisoned")
            .iter()
            .find(|(_, pty)| pty.session_id == session_id)
            .map(|(id, pty)| (id.clone(), Arc::clone(pty)))
            .context("session has no terminal job")?;
        let state = managed.state.lock().expect("PTY state poisoned");
        let replay_from = after_offset
            .unwrap_or(state.replay_start)
            .clamp(state.replay_start, state.replay_end);
        drop(state);
        let ticket = Uuid::new_v4().to_string();
        let mut stream = managed.stream.lock().expect("PTY stream state poisoned");
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
            .lock()
            .expect("PTY map poisoned")
            .iter()
            .map(|(id, pty)| (id.clone(), Arc::clone(pty)))
            .collect::<Vec<_>>();
        for (pty_id, managed) in entries {
            let mut stream = managed.stream.lock().expect("PTY stream state poisoned");
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
        let mut stream = managed.stream.lock().expect("PTY stream state poisoned");
        if stream.claimed.as_deref() == Some(claim.claim_id.as_str()) {
            stream.claimed = None;
        }
    }

    pub fn write_claimed(&self, claim: &PtyClaim, bytes: Vec<u8>) -> Result<()> {
        let managed = self.get_claimed(claim)?;
        let mut state = managed.state.lock().expect("PTY state poisoned");
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
        managed.state.lock().expect("PTY state poisoned").size = size;
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
        let mut state = managed.state.lock().expect("PTY state poisoned");
        let cursor = cursor.max(state.replay_start).min(state.replay_end);
        if cursor == state.replay_end && state.exit_status.is_none() && !state.shutdown {
            let (updated, _) = managed
                .changed
                .wait_timeout(state, wait)
                .expect("PTY state poisoned while waiting");
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
            .lock()
            .expect("PTY map poisoned")
            .remove(pty_id)
            .context("unknown terminal job")?;
        shutdown(&managed);
        Ok(())
    }

    pub fn terminate_session(&self, session_id: &str) {
        let removed = {
            let mut ptys = self.inner.lock().expect("PTY map poisoned");
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
        self.inner
            .lock()
            .expect("PTY map poisoned")
            .values()
            .any(|pty| {
                pty.session_id == session_id
                    && pty
                        .state
                        .lock()
                        .expect("PTY state poisoned")
                        .exit_status
                        .is_none()
            })
    }

    fn get(&self, pty_id: &str) -> Result<Arc<ManagedPty>> {
        self.inner
            .lock()
            .expect("PTY map poisoned")
            .get(pty_id)
            .cloned()
            .context("unknown terminal job")
    }

    fn get_claimed(&self, claim: &PtyClaim) -> Result<Arc<ManagedPty>> {
        let managed = self.get(&claim.pty_id)?;
        let claimed = managed
            .stream
            .lock()
            .expect("PTY stream state poisoned")
            .claimed
            .as_deref()
            == Some(claim.claim_id.as_str());
        if !claimed {
            bail!("PTY stream claim is no longer active");
        }
        Ok(managed)
    }

    fn remove_completed_for_session(&self, session_id: &str) {
        let completed = {
            let ptys = self.inner.lock().expect("PTY map poisoned");
            ptys.iter()
                .find(|(_, pty)| {
                    pty.session_id == session_id
                        && pty
                            .state
                            .lock()
                            .expect("PTY state poisoned")
                            .exit_status
                            .is_some()
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
                let state = managed.state.lock().expect("PTY state poisoned");
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
                    let mut state = managed.state.lock().expect("PTY state poisoned");
                    for _ in 0..chunk.input_accepted.min(state.input.len()) {
                        state.input.pop_front();
                    }
                    append_output(&mut state, chunk.output);
                    if let Some(status) = chunk.status {
                        state.exit_status = Some(status);
                    }
                    let finished = state.exit_status.is_some();
                    managed.changed.notify_all();
                    if finished {
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

fn finish(managed: &ManagedPty, status: String) {
    let mut state = managed.state.lock().expect("PTY state poisoned");
    state.exit_status.get_or_insert(status);
    managed.changed.notify_all();
}

fn shutdown(managed: &ManagedPty) {
    managed.state.lock().expect("PTY state poisoned").shutdown = true;
    managed.changed.notify_all();
}

fn descriptor(pty_id: &str, managed: &ManagedPty) -> PtyDescriptor {
    let stream = managed.stream.lock().expect("PTY stream state poisoned");
    let state = managed.state.lock().expect("PTY state poisoned");
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

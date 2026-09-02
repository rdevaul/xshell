use crate::{PtyExchangeResult, PtySize};
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;
use xshell_pty::{PtySize as ProcessSize, RemotePtyProcess};

const MAX_ACTIVE_PTYS: usize = 64;
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_EXCHANGE_WAIT: Duration = Duration::from_millis(250);
const MAX_DIMENSION: u16 = 1_000;
const MAX_TERMINAL_TYPE_BYTES: usize = 128;

#[derive(Clone, Default)]
pub struct PtyCoordinator {
    inner: Arc<Mutex<HashMap<String, Arc<ManagedPty>>>>,
}

struct ManagedPty {
    owner_client_id: String,
    session_id: String,
    stream: Mutex<StreamAuthorization>,
    process: Mutex<RemotePtyProcess>,
}

struct StreamAuthorization {
    ticket: Option<String>,
    claimed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyClaim {
    pub pty_id: String,
    pub owner_client_id: String,
}

impl PtyCoordinator {
    pub fn start(
        &self,
        owner_client_id: &str,
        session_id: &str,
        command: String,
        cwd: &Path,
        size: PtySize,
        terminal_type: Option<String>,
    ) -> Result<(String, String)> {
        validate_command(&command)?;
        validate_size(size)?;
        validate_terminal_type(terminal_type.as_deref())?;
        {
            let ptys = self.inner.lock().expect("PTY map poisoned");
            if ptys.len() >= MAX_ACTIVE_PTYS {
                bail!("PTY capacity is exhausted");
            }
            if ptys.values().any(|pty| pty.session_id == session_id) {
                bail!("session already has an active PTY");
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
            owner_client_id: owner_client_id.to_owned(),
            session_id: session_id.to_owned(),
            stream: Mutex::new(StreamAuthorization {
                ticket: Some(ticket.clone()),
                claimed: false,
            }),
            process: Mutex::new(process),
        });
        let mut ptys = self.inner.lock().expect("PTY map poisoned");
        if ptys.len() >= MAX_ACTIVE_PTYS || ptys.values().any(|pty| pty.session_id == session_id) {
            bail!("session acquired another PTY while this PTY was starting");
        }
        ptys.insert(pty_id.clone(), managed);
        Ok((pty_id, ticket))
    }

    pub fn exchange(
        &self,
        owner_client_id: &str,
        pty_id: &str,
        input: Vec<u8>,
        size: PtySize,
        wait_ms: u64,
    ) -> Result<PtyExchangeResult> {
        let managed = self.get_owned(owner_client_id, pty_id)?;
        if managed
            .stream
            .lock()
            .expect("PTY stream state poisoned")
            .claimed
        {
            bail!("PTY has switched to its duplex stream");
        }
        self.exchange_managed(managed, pty_id, input, size, wait_ms)
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
            if stream.ticket.as_deref() == Some(ticket) && !stream.claimed {
                stream.ticket = None;
                stream.claimed = true;
                return Ok(PtyClaim {
                    pty_id,
                    owner_client_id: managed.owner_client_id.clone(),
                });
            }
        }
        bail!("unknown or already claimed PTY stream ticket")
    }

    pub fn exchange_claimed(
        &self,
        claim: &PtyClaim,
        input: Vec<u8>,
        size: PtySize,
        wait_ms: u64,
    ) -> Result<PtyExchangeResult> {
        let managed = self.get_owned(&claim.owner_client_id, &claim.pty_id)?;
        if !managed
            .stream
            .lock()
            .expect("PTY stream state poisoned")
            .claimed
        {
            bail!("PTY stream has not been claimed");
        }
        self.exchange_managed(managed, &claim.pty_id, input, size, wait_ms)
    }

    fn exchange_managed(
        &self,
        managed: Arc<ManagedPty>,
        pty_id: &str,
        input: Vec<u8>,
        size: PtySize,
        wait_ms: u64,
    ) -> Result<PtyExchangeResult> {
        if input.len() > MAX_INPUT_BYTES {
            bail!("PTY input exceeds {MAX_INPUT_BYTES} bytes");
        }
        validate_size(size)?;
        let wait = Duration::from_millis(wait_ms);
        if wait > MAX_EXCHANGE_WAIT {
            bail!(
                "PTY exchange wait exceeds {} ms",
                MAX_EXCHANGE_WAIT.as_millis()
            );
        }
        let chunk = managed
            .process
            .lock()
            .expect("PTY process poisoned")
            .exchange(
                &input,
                ProcessSize {
                    rows: size.rows,
                    columns: size.columns,
                },
                wait,
                MAX_OUTPUT_BYTES,
            )?;
        let result = PtyExchangeResult {
            output: chunk.output,
            input_accepted: chunk.input_accepted,
            status: chunk.status,
        };
        if result.status.is_some() {
            self.inner.lock().expect("PTY map poisoned").remove(pty_id);
        }
        Ok(result)
    }

    pub fn close_claimed(&self, claim: &PtyClaim) {
        let removed = self
            .inner
            .lock()
            .expect("PTY map poisoned")
            .remove(&claim.pty_id);
        drop(removed);
    }

    pub fn close(&self, owner_client_id: &str, pty_id: &str) -> Result<()> {
        self.get_owned(owner_client_id, pty_id)?;
        let managed = self
            .inner
            .lock()
            .expect("PTY map poisoned")
            .remove(pty_id)
            .context("PTY disappeared while closing")?;
        drop(managed);
        Ok(())
    }

    pub fn close_owner(&self, owner_client_id: &str) {
        self.remove_matching(|pty| pty.owner_client_id == owner_client_id);
    }

    pub fn close_session(&self, session_id: &str) {
        self.remove_matching(|pty| pty.session_id == session_id);
    }

    fn remove_matching(&self, predicate: impl Fn(&ManagedPty) -> bool) {
        let removed = {
            let mut ptys = self.inner.lock().expect("PTY map poisoned");
            let ids = ptys
                .iter()
                .filter(|(_, pty)| predicate(pty))
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|id| ptys.remove(&id))
                .collect::<Vec<_>>()
        };
        drop(removed);
    }

    pub fn has_session(&self, session_id: &str) -> bool {
        self.inner
            .lock()
            .expect("PTY map poisoned")
            .values()
            .any(|pty| pty.session_id == session_id)
    }

    fn get_owned(&self, owner_client_id: &str, pty_id: &str) -> Result<Arc<ManagedPty>> {
        let managed = self
            .inner
            .lock()
            .expect("PTY map poisoned")
            .get(pty_id)
            .cloned()
            .context("unknown or completed PTY")?;
        if managed.owner_client_id != owner_client_id {
            bail!("PTY is owned by another client");
        }
        Ok(managed)
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
    fn coordinates_owned_pty_until_exit() {
        let temporary = TempDir::new().unwrap();
        let coordinator = PtyCoordinator::default();
        let size = PtySize {
            rows: 24,
            columns: 80,
        };
        let (id, ticket) = coordinator
            .start(
                "client-a",
                "session-a",
                "read value; printf 'remote:%s' \"$value\"".into(),
                temporary.path(),
                size,
                Some("xterm-256color".into()),
            )
            .unwrap();
        assert!(coordinator.has_session("session-a"));
        assert!(
            coordinator
                .exchange("client-b", &id, Vec::new(), size, 0)
                .is_err()
        );
        let claim = coordinator.claim(&ticket).unwrap();
        assert!(
            coordinator
                .exchange("client-a", &id, Vec::new(), size, 0)
                .is_err()
        );
        let mut pending = b"hello\n".to_vec();
        let mut output = Vec::new();
        loop {
            let result = coordinator
                .exchange_claimed(&claim, pending.clone(), size, 100)
                .unwrap();
            pending.drain(..result.input_accepted);
            output.extend(result.output);
            if result.status.is_some() {
                break;
            }
        }
        assert!(String::from_utf8_lossy(&output).contains("remote:hello"));
        assert!(!coordinator.has_session("session-a"));
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

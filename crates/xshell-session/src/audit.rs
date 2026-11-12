//! Execution-boundary auditing for `xshelld`.
//!
//! The daemon is the component that actually runs model turns, tool calls,
//! and `$` commands. Recording those events here, rather than in an attached
//! CLI, means they are captured even when no client is attached, they cannot
//! be lost to replay-journal truncation, and `required = true` is enforced by
//! the process that would otherwise perform the unaudited action.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use xshell_audit::{AuditClient, AuditConfig, AuditEvent};
use xshell_platform::LockExt;

/// Whether and how much of an interactive process's byte stream the daemon records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalStreamPolicy {
    /// Per-job capture budget in bytes (input + output). `None` is unbounded.
    pub max_bytes: Option<u64>,
}

/// Per-daemon audit policy plus one audit-service session per xshell session.
///
/// Each xshell session has its own connection behind its own lock, and every
/// exchange with the audit service has a deadline (`audit.ack_timeout_ms`).
/// The shared map is locked only to find or insert a session's entry, never
/// across I/O, so a stalled exchange delays only the session it belongs to,
/// and only until its deadline.
#[derive(Clone, Default)]
pub struct DaemonAudit {
    inner: Option<Arc<AuditInner>>,
}

struct AuditInner {
    config: AuditConfig,
    sessions: Mutex<HashMap<String, Arc<Mutex<SessionAudit>>>>,
}

/// One xshell session's audit connection.
struct SessionAudit {
    session_id: String,
    descriptor: SessionAuditDescriptor,
    state: Connection,
}

enum Connection {
    /// Not opened yet.
    Unopened,
    Open(AuditClient),
    /// The last open or exchange failed at this time. Nothing is recorded
    /// (and with `required = true`, nothing audited runs) until a reconnect
    /// succeeds; one is attempted on the first record after
    /// `ack_timeout_ms`, so a dead service costs each session at most one
    /// deadline per interval.
    Failed(Instant),
    /// The xshell session was closed; later records are dropped.
    Closed,
}

/// `LogicalSessionAttached` actions: a session's first audit connection, and
/// a later one replacing a connection that failed. Records made between the
/// two were not acknowledged; with `required = true` their actions did not
/// run.
const ATTACH_ACTION: &str = "xshelld_execution";
const RESUME_ACTION: &str = "xshelld_execution_resumed";

impl DaemonAudit {
    /// Build from configuration. When auditing is disabled this is a no-op
    /// sink; when enabled and `required`, connectivity is verified at startup
    /// so the daemon refuses to start without a reachable audit service.
    pub fn from_config(config: &AuditConfig) -> Result<Self> {
        if !config.enabled {
            return Ok(Self::default());
        }
        let socket = config
            .socket
            .as_deref()
            .context("audit.socket is required when auditing is enabled")?;
        if config.required {
            // Probe once so misconfiguration is reported at startup rather
            // than on the first turn. The probe session is closed cleanly.
            let mut probe = AuditClient::connect_with_timeout(
                socket,
                env!("CARGO_PKG_VERSION"),
                config.ack_timeout(),
            )
            .context("required auditing is unavailable; xshelld will not start")?;
            probe.append(AuditEvent::SessionEnded {
                reason: "xshelld audit probe".into(),
            })?;
            probe.close()?;
        }
        Ok(Self {
            inner: Some(Arc::new(AuditInner {
                config: config.clone(),
                sessions: Mutex::new(HashMap::new()),
            })),
        })
    }

    pub fn enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub fn required(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.config.required)
    }

    /// Terminal-stream capture policy for this daemon. `None` when auditing is
    /// off or stream capture is not enabled; otherwise the per-job byte budget
    /// (`None` inside means unbounded).
    pub fn terminal_stream(&self) -> Option<TerminalStreamPolicy> {
        let inner = self.inner.as_ref()?;
        if !inner.config.terminal_stream {
            return None;
        }
        Some(TerminalStreamPolicy {
            max_bytes: match inner.config.terminal_stream_max_bytes {
                0 => None,
                bytes => Some(bytes),
            },
        })
    }

    /// Return a handle for appending events on behalf of one xshell session,
    /// opening the audit-service session on first use.
    pub fn session(
        &self,
        session_id: &str,
        descriptor: SessionAuditDescriptor,
    ) -> SessionAuditHandle {
        let Some(inner) = &self.inner else {
            return SessionAuditHandle::disabled();
        };
        let entry = Arc::clone(
            inner
                .sessions
                .lock_recover()
                .entry(session_id.to_owned())
                .or_insert_with(|| {
                    Arc::new(Mutex::new(SessionAudit {
                        session_id: session_id.to_owned(),
                        descriptor,
                        state: Connection::Unopened,
                    }))
                }),
        );
        {
            let mut audit = entry.lock_recover();
            if matches!(audit.state, Connection::Unopened) {
                audit.open(&inner.config, ATTACH_ACTION);
            }
        }
        SessionAuditHandle {
            inner: Some(Arc::clone(inner)),
            entry: Some(entry),
        }
    }

    /// The audit-service session ID for an xshell session, if one is open.
    pub fn audit_session_id(&self, session_id: &str) -> Option<String> {
        let inner = self.inner.as_ref()?;
        let entry = inner.sessions.lock_recover().get(session_id).cloned()?;
        let audit = entry.lock_recover();
        match &audit.state {
            Connection::Open(client) => Some(client.session_id().to_owned()),
            _ => None,
        }
    }

    /// Close the audit-service session for a deleted xshell session.
    pub fn close_session(&self, session_id: &str, reason: &str) {
        let Some(inner) = &self.inner else {
            return;
        };
        let removed = inner.sessions.lock_recover().remove(session_id);
        if let Some(entry) = removed {
            entry.lock_recover().close(reason);
        }
    }

    /// Finalize every open audit session with a signed checkpoint. Called on
    /// daemon shutdown so a clean stop does not leave verifiable-but-open
    /// chains behind. Sessions close in parallel, each within its deadlines,
    /// so a stalled service delays shutdown by about one session's worth.
    pub fn close_all(&self, reason: &str) {
        let Some(inner) = &self.inner else {
            return;
        };
        let drained: Vec<_> = inner.sessions.lock_recover().drain().collect();
        std::thread::scope(|scope| {
            for (_, entry) in &drained {
                scope.spawn(|| entry.lock_recover().close(reason));
            }
        });
    }
}

impl SessionAudit {
    /// Open a connection and record the attachment. On failure the session
    /// is marked failed and the reason reported.
    fn open(&mut self, config: &AuditConfig, action: &str) {
        let opened = config
            .socket
            .as_deref()
            .context("audit.socket is required when auditing is enabled")
            .and_then(|socket| {
                AuditClient::connect_with_timeout(
                    socket,
                    env!("CARGO_PKG_VERSION"),
                    config.ack_timeout(),
                )
            })
            .and_then(|mut client| {
                client.append(AuditEvent::LogicalSessionAttached {
                    action: action.into(),
                    session_id: self.session_id.clone(),
                    name: self.descriptor.name.clone(),
                    host_id: self.descriptor.host_id.clone(),
                    host_alias: self.descriptor.host_alias.clone(),
                    user: self.descriptor.user.clone(),
                    terminal_stream: Some(config.terminal_stream),
                })?;
                Ok(client)
            });
        self.state = match opened {
            Ok(client) => Connection::Open(client),
            Err(error) => {
                eprintln!(
                    "xshelld audit warning: session {}: {error:#}",
                    self.session_id
                );
                Connection::Failed(Instant::now())
            }
        };
    }

    fn append(&mut self, config: &AuditConfig, event: AuditEvent) -> Result<Option<AuditReceipt>> {
        match self.state {
            Connection::Closed => return Ok(None),
            Connection::Unopened => self.open(config, ATTACH_ACTION),
            Connection::Failed(at) if at.elapsed() >= config.ack_timeout() => {
                self.open(config, RESUME_ACTION)
            }
            Connection::Open(_) | Connection::Failed(_) => {}
        }
        let Connection::Open(client) = &mut self.state else {
            if config.required {
                anyhow::bail!("required auditing is unavailable for this session");
            }
            return Ok(None);
        };
        match client.append(event) {
            Ok((sequence, record_hash)) => Ok(Some(AuditReceipt {
                audit_session_id: client.session_id().to_owned(),
                sequence,
                record_hash,
            })),
            Err(error) => {
                self.state = Connection::Failed(Instant::now());
                if config.required {
                    Err(error).context(
                        "required audit append failed; refusing to continue the audited action",
                    )
                } else {
                    eprintln!(
                        "xshelld audit warning: session {}: {error:#}; records are dropped until the audit service responds again",
                        self.session_id
                    );
                    Ok(None)
                }
            }
        }
    }

    fn close(&mut self, reason: &str) {
        if let Connection::Open(mut client) = std::mem::replace(&mut self.state, Connection::Closed)
        {
            let ended = client.append(AuditEvent::SessionEnded {
                reason: reason.into(),
            });
            if let Err(error) = ended.and_then(|_| client.close()) {
                eprintln!("xshelld audit warning: could not finalize audit: {error:#}");
            }
        }
    }
}

/// Identity fields recorded when the daemon first audits a session.
#[derive(Clone)]
pub struct SessionAuditDescriptor {
    pub name: String,
    pub host_id: String,
    pub host_alias: String,
    pub user: String,
}

/// Appends events for one xshell session. Cheap to clone; safe to call from
/// the turn thread.
#[derive(Clone)]
pub struct SessionAuditHandle {
    inner: Option<Arc<AuditInner>>,
    entry: Option<Arc<Mutex<SessionAudit>>>,
}

impl SessionAuditHandle {
    fn disabled() -> Self {
        Self {
            inner: None,
            entry: None,
        }
    }

    /// Append one event, waiting at most the acknowledgement deadline. With
    /// `required = true` a failure is returned so the caller can refuse to
    /// continue the action, and later appends fail at once until the audit
    /// service responds again; otherwise the failure is reported and the
    /// record dropped.
    pub fn append(&self, event: AuditEvent) -> Result<()> {
        self.append_with_receipt(event).map(|_| ())
    }

    /// As [`append`](Self::append), also saying where the record landed.
    /// `None` when auditing is off, or the record was dropped by
    /// best-effort auditing.
    pub fn append_with_receipt(&self, event: AuditEvent) -> Result<Option<AuditReceipt>> {
        let (Some(inner), Some(entry)) = (&self.inner, &self.entry) else {
            return Ok(None);
        };
        entry.lock_recover().append(&inner.config, event)
    }
}

/// Where one appended record landed in the audit log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditReceipt {
    pub audit_session_id: String,
    pub sequence: u64,
    pub record_hash: String,
}

/// The audit record of what settled a tool call.
pub fn audit_decision_basis(
    basis: &xshell_execution::DecisionBasis,
) -> xshell_audit::AuditDecisionBasis {
    use xshell_execution::DecisionBasis;
    let (list, entry) = match basis {
        DecisionBasis::Entry { list, entry, .. } => {
            (Some(list.as_str().to_owned()), Some(entry.clone()))
        }
        _ => (None, None),
    };
    xshell_audit::AuditDecisionBasis {
        kind: basis.kind().to_owned(),
        policy: basis.layer().map(|layer| layer.as_str().to_owned()),
        list,
        entry,
    }
}

/// The audit record of who answered a prompted call.
pub fn audit_approval(source: &xshell_execution::ApprovalSource) -> xshell_audit::AuditApproval {
    xshell_audit::AuditApproval {
        approver: source.approver.as_str().to_owned(),
        detail: source.detail.clone(),
    }
}

/// The audit `ToolDecision` record for an engine decision event.
pub fn audit_tool_decision(
    call_id: &str,
    decision: &str,
    basis: Option<&xshell_execution::DecisionBasis>,
    approval: Option<&xshell_execution::ApprovalSource>,
) -> xshell_audit::AuditEvent {
    xshell_audit::AuditEvent::ToolDecision {
        call_id: call_id.to_owned(),
        decision: decision.to_owned(),
        basis: basis.map(audit_decision_basis),
        approval: approval.map(audit_approval),
    }
}

/// The audit record of a policy, or `None` for an empty one.
pub fn audit_policy(policy: &xshell_execution::Policy) -> Option<xshell_audit::AuditPolicy> {
    Some(xshell_audit::AuditPolicy {
        sha256: policy.identity()?.to_owned(),
        policy: serde_json::to_value(policy.config())
            .expect("a policy configuration always serializes"),
    })
}

/// The audit `TurnStarted` record: the authority an agent turn runs under.
pub fn audit_turn_started(
    turn_id: &str,
    approval: xshell_execution::ApprovalPolicy,
    requested_approval: Option<xshell_execution::ApprovalPolicy>,
    limits: &xshell_execution::TurnLimits,
    requested_limits: Option<&xshell_execution::TurnLimits>,
    rules: &xshell_execution::ApprovalRules,
    tools: &xshell_execution::ToolRegistry,
) -> xshell_audit::AuditEvent {
    fn encode(value: impl serde::Serialize) -> serde_json::Value {
        serde_json::to_value(value).expect("approval modes and turn limits always serialize")
    }
    let mode = |mode| match encode(mode) {
        serde_json::Value::String(mode) => mode,
        other => other.to_string(),
    };
    xshell_audit::AuditEvent::TurnStarted {
        turn_id: turn_id.to_owned(),
        approval: mode(approval),
        requested_approval: requested_approval.map(mode),
        limits: encode(limits),
        requested_limits: requested_limits.map(|limits| Box::new(encode(limits))),
        base_policy: audit_policy(&rules.base).map(Box::new),
        overlay_policy: rules.overlay.as_ref().and_then(audit_policy).map(Box::new),
        provided_tools: audit_provided_tools(tools).map(Box::new),
    }
}

/// The audit form of a tool registry, or `None` when it provides no tools.
pub fn audit_provided_tools(
    tools: &xshell_execution::ToolRegistry,
) -> Option<xshell_audit::AuditProvidedTools> {
    Some(xshell_audit::AuditProvidedTools {
        sha256: tools.identity()?.to_owned(),
        tools: tools
            .tools()
            .iter()
            .map(|tool| xshell_audit::AuditProvidedTool {
                provider: tool.provider_name().to_owned(),
                name: tool.name().to_owned(),
                access: tool.access().as_str().to_owned(),
                approval: tool.approval().as_str().to_owned(),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;
    use std::time::Duration;
    use xshell_audit::{AUDIT_PROTOCOL_VERSION, ClientRequest};

    const DEADLINE: Duration = Duration::from_secs(1);

    /// An audit service that answers like `xshell-auditd` but never
    /// acknowledges a record whose text contains `STALL`, nor anything at all
    /// while `stall_all` is set. A withheld reply leaves the connection open,
    /// as a hung peer would.
    struct FakeAuditService {
        _directory: tempfile::TempDir,
        socket: PathBuf,
        stall_all: Arc<AtomicBool>,
        attachments: Arc<Mutex<Vec<String>>>,
    }

    impl FakeAuditService {
        fn start() -> Self {
            let directory = tempfile::TempDir::new().unwrap();
            let socket = directory.path().join("audit.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let stall_all = Arc::new(AtomicBool::new(false));
            let attachments = Arc::new(Mutex::new(Vec::new()));
            let (stall, attached) = (Arc::clone(&stall_all), Arc::clone(&attachments));
            thread::spawn(move || {
                let sessions = Arc::new(AtomicU64::new(0));
                for stream in listener.incoming().flatten() {
                    let (stall, attached, sessions) = (
                        Arc::clone(&stall),
                        Arc::clone(&attached),
                        Arc::clone(&sessions),
                    );
                    thread::spawn(move || serve(stream, &stall, &attached, &sessions));
                }
            });
            Self {
                _directory: directory,
                socket,
                stall_all,
                attachments,
            }
        }

        fn config(&self) -> AuditConfig {
            AuditConfig {
                enabled: true,
                required: true,
                socket: Some(self.socket.clone()),
                ack_timeout_ms: DEADLINE.as_millis() as u64,
                ..AuditConfig::default()
            }
        }
    }

    fn serve(
        stream: UnixStream,
        stall_all: &AtomicBool,
        attached: &Mutex<Vec<String>>,
        sessions: &AtomicU64,
    ) {
        let mut writer = stream.try_clone().unwrap();
        let mut sequence = 0;
        let session = format!("fake-{}", sessions.fetch_add(1, Ordering::SeqCst));
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { return };
            if stall_all.load(Ordering::SeqCst) || line.contains("STALL") {
                continue;
            }
            let reply = match serde_json::from_str::<ClientRequest>(&line).unwrap() {
                ClientRequest::Open { .. } => serde_json::json!({
                    "response": "opened",
                    "protocol_version": AUDIT_PROTOCOL_VERSION,
                    "session_id": session,
                    "signing_key_id": "fake",
                }),
                ClientRequest::Append { event } => {
                    if let AuditEvent::LogicalSessionAttached { action, .. } = event {
                        attached.lock().unwrap().push(action);
                    }
                    sequence += 1;
                    serde_json::json!({"response": "ack", "sequence": sequence, "record_hash": "h"})
                }
                ClientRequest::Close => serde_json::json!({
                    "response": "closed",
                    "checkpoint": {
                        "body": {
                            "format_version": 1, "session_id": session,
                            "checkpoint_sequence": 0, "previous_checkpoint_hash": "",
                            "sequence": sequence, "daemon_timestamp_unix_ms": 0,
                            "chain_head": "", "signing_key_id": "fake", "final_checkpoint": true,
                        },
                        "blinding_nonce": "", "witness": {"scheme": "", "commitment": ""},
                        "signature": "", "checkpoint_hash": "",
                    },
                }),
            };
            if writeln!(writer, "{reply}").is_err() {
                return;
            }
        }
    }

    fn descriptor(name: &str) -> SessionAuditDescriptor {
        SessionAuditDescriptor {
            name: name.into(),
            host_id: "host".into(),
            host_alias: "host".into(),
            user: "user".into(),
        }
    }

    fn stalling_event() -> AuditEvent {
        AuditEvent::SessionEnded {
            reason: "STALL".into(),
        }
    }

    fn ordinary_event() -> AuditEvent {
        AuditEvent::SessionEnded {
            reason: "ordinary".into(),
        }
    }

    #[test]
    fn f6_a_stalled_session_does_not_hold_up_another() {
        let service = FakeAuditService::start();
        let audit = DaemonAudit::from_config(&service.config()).unwrap();
        let stalled = audit.session("a", descriptor("a"));
        let other = audit.session("b", descriptor("b"));

        let started = Instant::now();
        let waiting = thread::spawn(move || stalled.append(stalling_event()));
        thread::sleep(Duration::from_millis(50));
        // While session a waits on its acknowledgement, session b records.
        other.append(ordinary_event()).unwrap();
        assert!(
            !waiting.is_finished(),
            "the stalled append should still be waiting"
        );
        assert!(
            started.elapsed() < DEADLINE,
            "session b waited on session a"
        );

        // Session a fails at its deadline, closed rather than frozen.
        let error = waiting.join().unwrap().unwrap_err();
        let elapsed = started.elapsed();
        assert!(elapsed >= DEADLINE && elapsed < DEADLINE * 4, "{elapsed:?}");
        assert!(format!("{error:#}").contains("required audit append failed"));
        assert!(
            format!("{error:#}").contains(&format!(
                "did not respond within {} ms",
                DEADLINE.as_millis()
            )),
            "{error:#}"
        );
    }

    #[test]
    fn f6_a_failed_session_refuses_at_once_then_resumes_on_a_new_connection() {
        let service = FakeAuditService::start();
        let audit = DaemonAudit::from_config(&service.config()).unwrap();
        let handle = audit.session("a", descriptor("a"));
        let first_audit_session = audit.audit_session_id("a").unwrap();
        handle.append(stalling_event()).unwrap_err();

        // Latched: the next record fails without waiting for the service.
        let started = Instant::now();
        let error = handle.append(ordinary_event()).unwrap_err();
        assert!(started.elapsed() < DEADLINE / 2);
        assert!(
            error
                .to_string()
                .contains("required auditing is unavailable")
        );
        assert_eq!(audit.audit_session_id("a"), None);

        // After the retry interval a new connection takes over, recorded as
        // a resumption.
        thread::sleep(DEADLINE);
        handle.append(ordinary_event()).unwrap();
        let resumed = audit.audit_session_id("a").unwrap();
        assert_ne!(resumed, first_audit_session);
        assert_eq!(
            *service.attachments.lock().unwrap(),
            [ATTACH_ACTION, RESUME_ACTION]
        );
    }

    #[test]
    fn f6_optional_audit_drops_records_without_stopping_the_action() {
        let service = FakeAuditService::start();
        let config = AuditConfig {
            required: false,
            ..service.config()
        };
        let audit = DaemonAudit::from_config(&config).unwrap();
        let handle = audit.session("a", descriptor("a"));
        handle.append(stalling_event()).unwrap();
        // Dropped at once while failed.
        let started = Instant::now();
        handle.append(ordinary_event()).unwrap();
        assert!(started.elapsed() < DEADLINE / 2);
    }

    #[test]
    fn f6_shutdown_with_a_hung_service_takes_one_deadline_not_one_per_session() {
        let service = FakeAuditService::start();
        let audit = DaemonAudit::from_config(&service.config()).unwrap();
        let handles: Vec<_> = (0..8)
            .map(|index| audit.session(&index.to_string(), descriptor("s")))
            .collect();
        for handle in &handles {
            handle.append(ordinary_event()).unwrap();
        }
        service.stall_all.store(true, Ordering::SeqCst);

        let started = Instant::now();
        audit.close_all("shutdown");
        let elapsed = started.elapsed();
        // Each close waits one deadline for its unacknowledged final record;
        // sequentially that would be eight.
        assert!(elapsed < DEADLINE * 3, "{elapsed:?}");
        // Closed sessions drop later records instead of reconnecting.
        handles[0].append(ordinary_event()).unwrap();
    }

    #[test]
    fn f6_closing_one_session_waits_only_for_that_session() {
        let service = FakeAuditService::start();
        let audit = DaemonAudit::from_config(&service.config()).unwrap();
        let stalled = audit.session("a", descriptor("a"));
        let other = audit.session("b", descriptor("b"));
        let waiting = thread::spawn(move || stalled.append(stalling_event()));
        thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        audit.close_session("b", "closed");
        other.append(ordinary_event()).unwrap();
        assert!(started.elapsed() < DEADLINE, "closing b waited on a");
        waiting.join().unwrap().unwrap_err();
    }

    struct OneTool;

    #[async_trait::async_trait]
    impl xshell_execution::ToolProvider for OneTool {
        fn name(&self) -> &str {
            "memory"
        }

        fn tools(&self) -> Vec<xshell_execution::ProvidedTool> {
            vec![xshell_execution::ProvidedTool {
                definition: xshell_core::ToolDefinition {
                    name: "memory_purge".into(),
                    description: "Forget everything.".into(),
                    parameters: serde_json::json!({"type": "object"}),
                },
                access: xshell_execution::ToolAccess::ReadWrite,
                approval: xshell_execution::ToolApproval::Required,
            }]
        }

        async fn call(
            &self,
            _tool: &str,
            _context: xshell_execution::ToolContext,
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    #[test]
    fn turn_records_name_provided_tools_only_when_there_are_some() {
        let rules = xshell_execution::ApprovalRules::default();
        let limits = xshell_execution::TurnLimits::default();
        let started = |tools: &xshell_execution::ToolRegistry| {
            serde_json::to_value(audit_turn_started(
                "t1",
                xshell_execution::ApprovalPolicy::Ask,
                None,
                &limits,
                None,
                &rules,
                tools,
            ))
            .unwrap()
        };
        // Without providers the record is unchanged, so it hashes as before.
        let plain = started(&xshell_execution::ToolRegistry::empty());
        assert!(plain.get("provided_tools").is_none(), "{plain}");

        let registry = xshell_execution::ToolRegistry::builder()
            .provider(Arc::new(OneTool))
            .build()
            .unwrap();
        let record = started(&registry);
        assert_eq!(
            record["provided_tools"],
            serde_json::json!({
                "sha256": registry.identity().unwrap(),
                "tools": [{
                    "provider": "memory",
                    "name": "memory_purge",
                    "access": "read_write",
                    "approval": "required",
                }],
            })
        );
    }
}

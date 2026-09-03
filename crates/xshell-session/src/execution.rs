use crate::audit::{DaemonAudit, SessionAuditDescriptor, SessionAuditHandle};
use crate::{
    ApprovalReply, EventBatch, SessionActivity, SessionEvent, SessionEventKind, SessionRegistry,
    TurnInput,
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;
use xshell_audit::AuditEvent;
use xshell_core::ToolCall;
use xshell_execution::{
    AdapterConfig, ApprovalDecision, ApprovalPolicy, CancellationFlag, ExecutionEvent, GateReason,
    SensitivePaths, TurnObserver, TurnPolicy, build_adapter, run_agent_turn,
    run_direct_shell_streaming,
};
use xshell_platform::LockExt;

const MAX_JOURNAL_EVENTS: usize = 8_192;
const MAX_JOURNAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_EVENT_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct ExecutionCoordinator {
    inner: Arc<CoordinatorInner>,
}

struct CoordinatorInner {
    registry: Arc<Mutex<SessionRegistry>>,
    sessions: Mutex<HashMap<String, Arc<SessionExecution>>>,
    audit: DaemonAudit,
    max_approval: ApprovalPolicy,
    sensitive_paths: SensitivePaths,
}

struct SessionExecution {
    state: Mutex<ExecutionState>,
    /// Wakes synchronous waiters: client request threads blocked in
    /// `events()` long-polls.
    changed: Condvar,
    /// Wakes the asynchronous waiter: the turn task blocked in `approve()`
    /// waiting for a client decision. Separate from `changed` because a
    /// Condvar cannot be awaited from a future.
    approvals_ready: tokio::sync::Notify,
}

struct ExecutionState {
    events: VecDeque<SessionEvent>,
    event_bytes: usize,
    next_sequence: u64,
    active: Option<ActiveTurn>,
    approvals: HashMap<(String, String), ApprovalDecision>,
    pending_approvals: HashSet<(String, String)>,
}

struct ActiveTurn {
    id: String,
    cancellation: CancellationFlag,
}

impl ExecutionCoordinator {
    pub fn new(registry: Arc<Mutex<SessionRegistry>>) -> Self {
        Self::with_policy(
            registry,
            DaemonAudit::default(),
            ApprovalPolicy::Ask,
            SensitivePaths::default(),
        )
    }

    pub fn with_policy(
        registry: Arc<Mutex<SessionRegistry>>,
        audit: DaemonAudit,
        max_approval: ApprovalPolicy,
        sensitive_paths: SensitivePaths,
    ) -> Self {
        Self {
            inner: Arc::new(CoordinatorInner {
                registry,
                sessions: Mutex::new(HashMap::new()),
                audit,
                max_approval,
                sensitive_paths,
            }),
        }
    }

    pub fn max_approval(&self) -> ApprovalPolicy {
        self.inner.max_approval
    }

    pub fn audit(&self) -> &DaemonAudit {
        &self.inner.audit
    }

    fn audit_handle_for_snapshot(&self, snapshot: &crate::SessionSnapshot) -> SessionAuditHandle {
        let descriptor = &snapshot.descriptor;
        self.inner.audit.session(
            &descriptor.id,
            SessionAuditDescriptor {
                name: descriptor.name.clone(),
                host_id: descriptor.host_id.clone(),
                host_alias: descriptor.host_alias.clone(),
                user: descriptor.user.clone(),
            },
        )
    }

    /// Open or retrieve the daemon-owned audit stream for a session. PTY jobs
    /// use the same stream as agent and direct-shell turns so protocol clients
    /// cannot bypass execution-boundary auditing.
    pub fn audit_handle(&self, session_id: &str) -> Result<SessionAuditHandle> {
        let snapshot = self.inner.registry.lock_recover().snapshot(session_id)?;
        Ok(self.audit_handle_for_snapshot(&snapshot))
    }

    pub fn submit(
        &self,
        session_id: &str,
        input: TurnInput,
        requested_approval: ApprovalPolicy,
    ) -> Result<String> {
        // The daemon executes; the daemon decides how much unattended
        // execution it permits. A client may ask for less, never more.
        let approval = requested_approval.clamp_to(self.inner.max_approval);
        let requested_approval = (approval != requested_approval).then_some(requested_approval);
        let snapshot = self.inner.registry.lock_recover().snapshot(session_id)?;
        let execution = self.session(session_id);
        let turn_id = Uuid::new_v4().to_string();
        let cancellation = CancellationFlag::default();
        // Record the input at the execution boundary before anything runs.
        // With required auditing, a failure here refuses the turn outright.
        let audit = self.audit_handle_for_snapshot(&snapshot);
        let (route, text) = match &input {
            TurnInput::Agent { message } => ("agent", message.clone()),
            TurnInput::Shell { command } => ("shell", format!("${command}")),
        };
        audit.append(AuditEvent::Input {
            route: route.into(),
            text,
        })?;
        {
            let mut state = execution.state.lock_recover();
            if let Some(active) = &state.active {
                bail!("session already has active turn {}", active.id);
            }
            state.events.clear();
            state.event_bytes = 0;
            state.approvals.clear();
            state.pending_approvals.clear();
            state.active = Some(ActiveTurn {
                id: turn_id.clone(),
                cancellation: cancellation.clone(),
            });
        }
        execution.append(
            &turn_id,
            SessionEventKind::TurnStarted {
                input: input.clone(),
                approval,
                requested_approval,
            },
        );

        let coordinator = self.clone();
        let session_id = session_id.to_owned();
        let spawned_turn_id = turn_id.clone();
        if let Err(error) = thread::Builder::new()
            .name(format!("xshell-turn-{}", &turn_id[..8]))
            .spawn(move || {
                coordinator.run_turn(
                    session_id,
                    spawned_turn_id,
                    input,
                    approval,
                    snapshot,
                    cancellation,
                    audit,
                );
            })
        {
            execution.append(
                &turn_id,
                SessionEventKind::TurnFailed {
                    message: error.to_string(),
                },
            );
            execution.finish(&turn_id);
            return Err(error).context("cannot start session turn");
        }
        Ok(turn_id)
    }

    pub fn events(&self, session_id: &str, after_sequence: u64, wait_ms: u64) -> EventBatch {
        let execution = self.session(session_id);
        execution.events(
            after_sequence,
            Duration::from_millis(wait_ms).min(MAX_EVENT_WAIT),
        )
    }

    pub fn approve(&self, session_id: &str, reply: ApprovalReply) -> Result<()> {
        let execution = self.session(session_id);
        let mut state = execution.state.lock_recover();
        if state.active.as_ref().map(|turn| turn.id.as_str()) != Some(reply.turn_id.as_str()) {
            bail!("turn is no longer active");
        }
        let key = (reply.turn_id, reply.call_id);
        if !state.pending_approvals.remove(&key) {
            bail!("tool call is not waiting for approval");
        }
        state.approvals.insert(key, reply.decision);
        execution.changed.notify_all();
        execution.approvals_ready.notify_waiters();
        Ok(())
    }

    pub fn cancel(&self, session_id: &str, turn_id: &str) -> Result<()> {
        let execution = self.session(session_id);
        let state = execution.state.lock_recover();
        let active = state
            .active
            .as_ref()
            .context("session has no active turn")?;
        if active.id != turn_id {
            bail!("turn {turn_id:?} is not active");
        }
        active.cancellation.cancel();
        execution.changed.notify_all();
        Ok(())
    }

    pub fn cancel_and_remove(&self, session_id: &str) {
        let execution = self.inner.sessions.lock_recover().remove(session_id);
        if let Some(execution) = execution
            && let Some(active) = &execution.state.lock_recover().active
        {
            active.cancellation.cancel();
            execution.changed.notify_all();
        }
    }

    pub fn active_turn(&self, session_id: &str) -> Option<String> {
        self.session(session_id)
            .state
            .lock_recover()
            .active
            .as_ref()
            .map(|turn| turn.id.clone())
    }

    pub fn activity(&self, session_id: &str) -> SessionActivity {
        let execution = self.session(session_id);
        let state = execution.state.lock_recover();
        if state.active.is_none() {
            SessionActivity::Idle
        } else if state.pending_approvals.is_empty() {
            SessionActivity::Running
        } else {
            SessionActivity::WaitingApproval
        }
    }

    fn session(&self, session_id: &str) -> Arc<SessionExecution> {
        self.inner
            .sessions
            .lock_recover()
            .entry(session_id.to_owned())
            .or_insert_with(|| Arc::new(SessionExecution::new()))
            .clone()
    }

    #[allow(clippy::too_many_arguments)]
    fn run_turn(
        &self,
        session_id: String,
        turn_id: String,
        input: TurnInput,
        approval: ApprovalPolicy,
        mut snapshot: crate::SessionSnapshot,
        cancellation: CancellationFlag,
        audit: SessionAuditHandle,
    ) {
        let execution = self.session(&session_id);
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                execution.append(
                    &turn_id,
                    SessionEventKind::TurnFailed {
                        message: error.to_string(),
                    },
                );
                execution.finish(&turn_id);
                return;
            }
        };
        let mut observer = DaemonObserver {
            turn_id: turn_id.clone(),
            execution: Arc::clone(&execution),
            cancellation: cancellation.clone(),
            audit: audit.clone(),
            audit_failure: None,
        };
        let result = runtime.block_on(async {
            match input {
                TurnInput::Agent { message } => {
                    let model = &snapshot.descriptor.model;
                    let mut agent = build_adapter(&AdapterConfig {
                        provider: model.provider.clone(),
                        model: model.model.clone(),
                        base_url: model.base_url.clone(),
                        api_key_env: model.api_key_env.clone(),
                    })?;
                    let policy = TurnPolicy::new(approval)
                        .with_sensitive_paths(self.inner.sensitive_paths.clone());
                    let outcome = run_agent_turn(
                        agent.as_mut(),
                        &mut snapshot.history,
                        message,
                        &snapshot.descriptor.cwd,
                        &policy,
                        &mut observer,
                    )
                    .await;
                    if let Err(error) = &outcome {
                        let _ = audit.append(AuditEvent::AgentError {
                            message: format!("{error:#}"),
                        });
                    }
                    outcome
                }
                TurnInput::Shell { command } => {
                    let result = tokio::select! {
                        result = run_direct_shell_streaming(
                            &command,
                            &snapshot.descriptor.cwd,
                            |stream, text| execution.append(
                                &turn_id,
                                SessionEventKind::ShellOutput {
                                    stream: stream.into(),
                                    text,
                                },
                            ),
                        ) => result?,
                        () = cancellation.wait() => bail!("shell command cancelled"),
                    };
                    if result.cwd != snapshot.descriptor.cwd {
                        snapshot.descriptor.cwd = result.cwd.clone();
                        audit.append(AuditEvent::WorkingDirectoryChanged {
                            cwd: result.cwd.display().to_string(),
                        })?;
                        execution.append(
                            &turn_id,
                            SessionEventKind::WorkingDirectoryChanged { cwd: result.cwd },
                        );
                    }
                    audit.append(AuditEvent::ShellFinished {
                        command: command.clone(),
                        outcome: result.status.clone(),
                        cwd: snapshot.descriptor.cwd.display().to_string(),
                    })?;
                    execution.append(
                        &turn_id,
                        SessionEventKind::ShellFinished {
                            command,
                            status: result.status,
                        },
                    );
                    Ok(())
                }
            }
        });

        // A required-audit failure inside the observer cancels the turn so
        // no further tool runs without a record. Report it as a failure, not
        // a user cancellation.
        let audit_failure = observer.audit_failure.take();
        if let Some(error) = audit_failure {
            execution.append(
                &turn_id,
                SessionEventKind::TurnFailed {
                    message: format!("{error:#}"),
                },
            );
        } else if cancellation.is_cancelled() {
            execution.append(&turn_id, SessionEventKind::TurnCancelled);
        } else if let Err(error) = result {
            execution.append(
                &turn_id,
                SessionEventKind::TurnFailed {
                    message: format!("{error:#}"),
                },
            );
        } else {
            let update = self.inner.registry.lock_recover().update_execution_state(
                &session_id,
                snapshot.descriptor.cwd,
                snapshot.history,
            );
            match update {
                Ok(_) => execution.append(&turn_id, SessionEventKind::TurnCompleted),
                Err(error) => execution.append(
                    &turn_id,
                    SessionEventKind::TurnFailed {
                        message: format!("cannot persist completed turn: {error:#}"),
                    },
                ),
            }
        }
        execution.finish(&turn_id);
    }
}

impl SessionExecution {
    fn new() -> Self {
        Self {
            state: Mutex::new(ExecutionState {
                events: VecDeque::new(),
                event_bytes: 0,
                next_sequence: 1,
                active: None,
                approvals: HashMap::new(),
                pending_approvals: HashSet::new(),
            }),
            changed: Condvar::new(),
            approvals_ready: tokio::sync::Notify::new(),
        }
    }

    fn append(&self, turn_id: &str, event: SessionEventKind) {
        let mut state = self.state.lock_recover();
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.saturating_add(1);
        let record = SessionEvent {
            sequence,
            turn_id: turn_id.to_owned(),
            timestamp_unix_ms: timestamp_ms(),
            event,
        };
        state.event_bytes = state
            .event_bytes
            .saturating_add(serde_json::to_vec(&record).map_or(0, |encoded| encoded.len()));
        state.events.push_back(record);
        while state.events.len() > MAX_JOURNAL_EVENTS || state.event_bytes > MAX_JOURNAL_BYTES {
            if let Some(removed) = state.events.pop_front() {
                state.event_bytes = state.event_bytes.saturating_sub(
                    serde_json::to_vec(&removed).map_or(0, |encoded| encoded.len()),
                );
            }
        }
        self.changed.notify_all();
    }

    fn events(&self, after_sequence: u64, wait: Duration) -> EventBatch {
        let deadline = Instant::now() + wait;
        let mut state = self.state.lock_recover();
        while !state
            .events
            .iter()
            .any(|event| event.sequence > after_sequence)
            && state.active.is_some()
            && Instant::now() < deadline
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let waited = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = waited.0;
            if waited.1.timed_out() {
                break;
            }
        }
        let first_sequence = state.events.front().map(|event| event.sequence);
        EventBatch {
            events: state
                .events
                .iter()
                .filter(|event| event.sequence > after_sequence)
                .cloned()
                .collect(),
            truncated_before: first_sequence
                .filter(|first| after_sequence.saturating_add(1) < *first),
            next_sequence: state.next_sequence,
            active_turn_id: state.active.as_ref().map(|turn| turn.id.clone()),
        }
    }

    fn finish(&self, turn_id: &str) {
        let mut state = self.state.lock_recover();
        if state.active.as_ref().map(|turn| turn.id.as_str()) == Some(turn_id) {
            state.active = None;
        }
        state
            .pending_approvals
            .retain(|(pending_turn, _)| pending_turn != turn_id);
        state
            .approvals
            .retain(|(pending_turn, _), _| pending_turn != turn_id);
        self.changed.notify_all();
    }
}

struct DaemonObserver {
    turn_id: String,
    execution: Arc<SessionExecution>,
    cancellation: CancellationFlag,
    audit: SessionAuditHandle,
    audit_failure: Option<anyhow::Error>,
}

impl DaemonObserver {
    fn audit(&mut self, event: AuditEvent) {
        if self.audit_failure.is_some() {
            return;
        }
        if let Err(error) = self.audit.append(event) {
            // Stop the turn at the next cancellation check so that no tool
            // executes without an audit record.
            self.audit_failure = Some(error);
            self.cancellation.cancel();
        }
    }
}

#[async_trait]
impl TurnObserver for DaemonObserver {
    fn emit(&mut self, event: ExecutionEvent) {
        match &event {
            ExecutionEvent::TextDelta { .. } | ExecutionEvent::ApprovalRequested { .. } => {}
            ExecutionEvent::AgentResponse {
                content,
                tool_call_count,
                partial,
            } => self.audit(AuditEvent::AgentResponse {
                content: content.clone(),
                tool_call_count: *tool_call_count,
                partial: *partial,
            }),
            ExecutionEvent::ToolRequested { call } => self.audit(AuditEvent::ToolRequested {
                call_id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            }),
            ExecutionEvent::ToolDecision { call_id, decision } => {
                self.audit(AuditEvent::ToolDecision {
                    call_id: call_id.clone(),
                    decision: match decision {
                        ApprovalDecision::Approve => "approve",
                        ApprovalDecision::Deny => "deny",
                        ApprovalDecision::AbortTurn => "abort_turn",
                    }
                    .into(),
                })
            }
            ExecutionEvent::ToolSkipped { call_id, .. } => self.audit(AuditEvent::ToolDecision {
                call_id: call_id.clone(),
                decision: "skipped_after_abort".into(),
            }),
            ExecutionEvent::ToolResult {
                call_id,
                name,
                result,
            } => self.audit(AuditEvent::ToolResult {
                call_id: call_id.clone(),
                name: name.clone(),
                result: result.clone(),
            }),
            ExecutionEvent::TurnAborted => {}
        }
        if let ExecutionEvent::ApprovalRequested { call, .. } = &event {
            self.execution
                .state
                .lock_recover()
                .pending_approvals
                .insert((self.turn_id.clone(), call.id.clone()));
        }
        self.execution
            .append(&self.turn_id, SessionEventKind::Execution { event });
    }

    fn cancellation(&self) -> CancellationFlag {
        self.cancellation.clone()
    }

    async fn approve(&mut self, call: &ToolCall, _reason: GateReason) -> ApprovalDecision {
        let key = (self.turn_id.clone(), call.id.clone());
        loop {
            if self.cancellation.is_cancelled() {
                return ApprovalDecision::AbortTurn;
            }
            // Register for the wake-up before checking, so a reply that
            // arrives between the check and the await is not missed.
            let notified = self.execution.approvals_ready.notified();
            if let Some(decision) = self.execution.state.lock_recover().approvals.remove(&key) {
                return decision;
            }
            tokio::select! {
                () = notified => {}
                () = self.cancellation.wait() => {}
            }
        }
    }
}

fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

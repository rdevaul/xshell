use crate::audit::{AuditReceipt, DaemonAudit, SessionAuditDescriptor, SessionAuditHandle};
use crate::{
    AgentTurnPhase, ApprovalReply, EventBatch, SessionActivity, SessionEvent, SessionEventKind,
    SessionRegistry, TurnInput,
};
use crate::{TurnAuditRef, TurnFailureReason};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;
use xshell_audit::AuditEvent;
use xshell_core::ToolCall;
use xshell_execution::{
    AdapterConfig, ApprovalAnswer, ApprovalDecision, ApprovalPolicy, ApprovalRules, ApprovalSource,
    Approver, BudgetExhausted, CancellationFlag, CompactionConfig, ExecutionEvent, GateReason,
    ModelBudget, Policy, SensitivePaths, ToolRegistry, ToolSet, TurnBudget, TurnLimits,
    TurnObserver, TurnPolicy, TurnUsage, build_adapter, run_agent_turn_with_budget,
    run_direct_shell_streaming_cancellable,
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
    admission: Mutex<bool>,
    workers: Mutex<Vec<thread::JoinHandle<()>>>,
    /// Turns running across all sessions, against
    /// `DaemonPolicy::max_concurrent_turns`.
    running_turns: Arc<AtomicUsize>,
    audit: DaemonAudit,
    policy: DaemonPolicy,
}

/// What the daemon's operator decides for every turn, whatever a client asks.
#[derive(Debug, Clone)]
pub struct DaemonPolicy {
    pub max_approval: ApprovalPolicy,
    pub max_limits: TurnLimits,
    pub sensitive_paths: SensitivePaths,
    pub blocked_paths: SensitivePaths,
    pub rules: Policy,
    pub compaction: CompactionConfig,
    /// Tools that compiled-in providers add to every agent turn.
    pub tools: Arc<ToolRegistry>,
    /// Most turns running at once across all sessions.
    pub max_concurrent_turns: usize,
}

impl Default for DaemonPolicy {
    fn default() -> Self {
        Self {
            max_approval: ApprovalPolicy::Ask,
            max_limits: TurnLimits::relaxed(),
            sensitive_paths: SensitivePaths::default(),
            blocked_paths: SensitivePaths::none(),
            rules: Policy::default(),
            compaction: CompactionConfig::default(),
            tools: ToolRegistry::empty(),
            max_concurrent_turns: crate::DEFAULT_MAX_CONCURRENT_TURNS,
        }
    }
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
    approvals: HashMap<(String, String), ApprovalAnswer>,
    pending_approvals: HashSet<(String, String)>,
}

struct ActiveTurn {
    id: String,
    cancellation: CancellationFlag,
    /// Held for as long as the turn is active: releasing the turn releases
    /// its place in the daemon-wide budget, on every path.
    _slot: TurnSlot,
}

/// One place in the daemon's concurrent-turn budget, given back when
/// dropped.
struct TurnSlot(Arc<AtomicUsize>);

impl TurnSlot {
    /// Take a place, unless `limit` turns are already running.
    fn acquire(running: &Arc<AtomicUsize>, limit: usize) -> Option<Self> {
        running
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < limit).then_some(count + 1)
            })
            .ok()
            .map(|_| Self(Arc::clone(running)))
    }
}

impl Drop for TurnSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl ExecutionCoordinator {
    pub fn new(registry: Arc<Mutex<SessionRegistry>>) -> Self {
        Self::with_policy(registry, DaemonAudit::default(), DaemonPolicy::default())
    }

    pub fn with_policy(
        registry: Arc<Mutex<SessionRegistry>>,
        audit: DaemonAudit,
        policy: DaemonPolicy,
    ) -> Self {
        Self {
            inner: Arc::new(CoordinatorInner {
                registry,
                sessions: Mutex::new(HashMap::new()),
                admission: Mutex::new(true),
                workers: Mutex::new(Vec::new()),
                running_turns: Arc::new(AtomicUsize::new(0)),
                audit,
                policy,
            }),
        }
    }

    pub fn max_approval(&self) -> ApprovalPolicy {
        self.inner.policy.max_approval
    }

    pub fn max_limits(&self) -> TurnLimits {
        self.inner.policy.max_limits
    }

    pub fn max_concurrent_turns(&self) -> usize {
        self.inner.policy.max_concurrent_turns
    }

    /// Turns running now across all sessions.
    pub fn running_turns(&self) -> usize {
        self.inner.running_turns.load(Ordering::Acquire)
    }

    /// The tools providers add to every agent turn.
    pub fn tools(&self) -> &ToolRegistry {
        &self.inner.policy.tools
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
        requested_limits: Option<TurnLimits>,
        overlay: Option<Policy>,
    ) -> Result<String> {
        if !*self.inner.admission.lock_recover() {
            bail!("session execution is shutting down");
        }
        self.reap_finished_workers();
        // The daemon executes; the daemon decides how much unattended
        // execution it permits. A client may ask for less, never more.
        let approval = requested_approval.clamp_to(self.inner.policy.max_approval);
        let requested_approval = (approval != requested_approval).then_some(requested_approval);
        // Limits work the same way: each one is lowered to the daemon's.
        let requested_limits = requested_limits.unwrap_or_default();
        let (limits, lowered) = requested_limits.clamp_to(self.inner.policy.max_limits);
        let requested_limits = lowered.then_some(requested_limits);
        let snapshot = self.inner.registry.lock_recover().snapshot(session_id)?;
        // Attached images must be this session's own, exactly as the daemon
        // ingested them, and only a model that takes images can see them.
        if let TurnInput::Agent { images, .. } = &input
            && !images.is_empty()
        {
            if !snapshot.descriptor.model.vision {
                bail!(
                    "this session's model is not configured for images; set `vision = true` \
on its model profile to attach images"
                );
            }
            self.inner
                .registry
                .lock_recover()
                .image_store(session_id)?
                .require_all(images)
                .context("the message attaches an image this session does not have")?;
        }
        let execution = self.session(session_id);
        let turn_id = Uuid::new_v4().to_string();
        let cancellation = CancellationFlag::default();
        {
            let mut state = execution.state.lock_recover();
            if let Some(active) = &state.active {
                bail!("session already has active turn {}", active.id);
            }
            let limit = self.inner.policy.max_concurrent_turns;
            let Some(slot) = TurnSlot::acquire(&self.inner.running_turns, limit) else {
                return Err(crate::CapacityExhausted {
                    message: format!(
                        "this host is already running {limit} turns, its limit \
(session_fabric.max_concurrent_turns); try again when one finishes"
                    ),
                }
                .into());
            };
            state.approvals.clear();
            state.pending_approvals.clear();
            state.active = Some(ActiveTurn {
                id: turn_id.clone(),
                cancellation: cancellation.clone(),
                _slot: slot,
            });
        }
        // Record the input at the execution boundary before anything runs.
        // The active-turn reservation above makes this an accepted input, not
        // merely an attempted duplicate submission. With required auditing, a
        // failure here releases the reservation and refuses the turn outright.
        let audit = self.audit_handle_for_snapshot(&snapshot);
        let (route, text, images) = match &input {
            TurnInput::Agent { message, images } => (
                "agent",
                message.clone(),
                images.iter().map(crate::audit_image).collect(),
            ),
            TurnInput::Shell { command } => ("shell", format!("${command}"), Vec::new()),
        };
        let first_record = match audit.append_with_receipt(AuditEvent::Input {
            route: route.into(),
            text,
            images,
        }) {
            Ok(receipt) => receipt,
            Err(error) => {
                execution.finish(&turn_id);
                return Err(error);
            }
        };
        // An agent turn's authority is recorded before anything can run
        // under it: decisions later in the log are read against it.
        if matches!(input, TurnInput::Agent { .. }) {
            let rules = ApprovalRules {
                base: self.inner.policy.rules.clone(),
                overlay: overlay.clone(),
            };
            if let Err(error) = audit.append(crate::audit_turn_started(
                &turn_id,
                approval,
                requested_approval,
                &limits,
                requested_limits.as_ref(),
                &rules,
                &self.inner.policy.tools,
            )) {
                execution.finish(&turn_id);
                return Err(error);
            }
        }
        if let Err(error) = self.inner.registry.lock_recover().begin_execution(
            &snapshot.descriptor.id,
            &turn_id,
            route,
        ) {
            execution.finish(&turn_id);
            return Err(error).context("cannot persist accepted session operation");
        }
        execution.append(
            &turn_id,
            SessionEventKind::TurnStarted {
                input: input.clone(),
                approval,
                requested_approval,
                limits: matches!(input, TurnInput::Agent { .. }).then_some(limits),
                requested_limits: requested_limits
                    .filter(|_| matches!(input, TurnInput::Agent { .. })),
            },
        );

        let coordinator = self.clone();
        let session_id = session_id.to_owned();
        let marker_session_id = session_id.clone();
        let spawned_turn_id = turn_id.clone();
        let admission = self.inner.admission.lock_recover();
        if !*admission {
            self.inner
                .registry
                .lock_recover()
                .finish_execution(&snapshot.descriptor.id, &turn_id)?;
            execution.conclude(&turn_id, SessionEventKind::TurnCancelled);
            bail!("session execution is shutting down");
        }
        let worker = thread::Builder::new()
            .name(format!("xshell-turn-{}", &turn_id[..8]))
            .spawn(move || {
                coordinator.run_turn(
                    session_id,
                    spawned_turn_id,
                    input,
                    approval,
                    limits,
                    overlay,
                    snapshot,
                    cancellation,
                    audit,
                    first_record,
                );
            });
        let worker = match worker {
            Ok(worker) => worker,
            Err(error) => {
                let persistence = self
                    .inner
                    .registry
                    .lock_recover()
                    .finish_execution(&marker_session_id, &turn_id);
                execution.conclude(
                    &turn_id,
                    SessionEventKind::TurnFailed {
                        message: error.to_string(),
                        reason: None,
                    },
                );
                persistence.context("cannot clear unstarted session operation")?;
                return Err(error).context("cannot start session turn");
            }
        };
        self.inner.workers.lock_recover().push(worker);
        drop(admission);
        Ok(turn_id)
    }

    pub fn stop_accepting_and_cancel(&self) {
        *self.inner.admission.lock_recover() = false;
        let sessions = self
            .inner
            .sessions
            .lock_recover()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for execution in sessions {
            if let Some(active) = &execution.state.lock_recover().active {
                active.cancellation.cancel();
                execution.changed.notify_all();
                execution.approvals_ready.notify_waiters();
            }
        }
    }

    pub fn wait_for_shutdown(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            self.reap_finished_workers();
            if self.inner.workers.lock_recover().is_empty() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn reap_finished_workers(&self) {
        let finished = {
            let mut workers = self.inner.workers.lock_recover();
            let mut pending = Vec::with_capacity(workers.len());
            let mut finished = Vec::new();
            for worker in workers.drain(..) {
                if worker.is_finished() {
                    finished.push(worker);
                } else {
                    pending.push(worker);
                }
            }
            *workers = pending;
            finished
        };
        for worker in finished {
            let _ = worker.join();
        }
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
        // The client says who answered. Its claim is checked before the
        // prompt is consumed, so a rejected reply leaves the call waiting.
        let source = validated_source(reply.decision, reply.source)?;
        let key = (reply.turn_id, reply.call_id);
        if !state.pending_approvals.remove(&key) {
            bail!("tool call is not waiting for approval");
        }
        state
            .approvals
            .insert(key, ApprovalAnswer::new(reply.decision, source));
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
        let Some(active) = &state.active else {
            return SessionActivity::Idle;
        };
        if active.cancellation.is_cancelled() {
            SessionActivity::AgentTurn {
                turn_id: active.id.clone(),
                phase: AgentTurnPhase::Stopping,
            }
        } else if state.pending_approvals.is_empty() {
            SessionActivity::AgentTurn {
                turn_id: active.id.clone(),
                phase: AgentTurnPhase::Running,
            }
        } else {
            SessionActivity::AgentTurn {
                turn_id: active.id.clone(),
                phase: AgentTurnPhase::WaitingApproval,
            }
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
        limits: TurnLimits,
        overlay: Option<Policy>,
        mut snapshot: crate::SessionSnapshot,
        cancellation: CancellationFlag,
        audit: SessionAuditHandle,
        first_record: Option<AuditReceipt>,
    ) {
        let execution = self.session(&session_id);
        let agent_turn = matches!(input, TurnInput::Agent { .. });
        let budget = TurnBudget::new(limits);
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                execution.conclude(
                    &turn_id,
                    SessionEventKind::TurnFailed {
                        message: error.to_string(),
                        reason: None,
                    },
                );
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
                TurnInput::Agent { message, images } => {
                    let model = &snapshot.descriptor.model;
                    let mut agent = build_adapter(&AdapterConfig {
                        provider: model.provider.clone(),
                        model: model.model.clone(),
                        base_url: model.base_url.clone(),
                        api_key_env: model.api_key_env.clone(),
                        vision: model.vision,
                    })?;
                    let mut policy = TurnPolicy::new(approval)
                        .with_tools(
                            ToolSet::all().with_provided(Arc::clone(&self.inner.policy.tools)),
                        )
                        .with_session_id(session_id.clone())
                        .with_limits(limits)
                        .with_rules(ApprovalRules {
                            base: self.inner.policy.rules.clone(),
                            overlay,
                        })
                        .with_sensitive_paths(self.inner.policy.sensitive_paths.clone())
                        .with_blocked_paths(self.inner.policy.blocked_paths.clone())
                        .with_compaction(&self.inner.policy.compaction.for_model(ModelBudget {
                            max_history_bytes: model.max_history_bytes,
                            context_tokens: model.context_tokens,
                            reserve_output_tokens: model.reserve_output_tokens,
                        }))
                        .with_message_images(images);
                    // The session's own store: a turn can only show its model
                    // images stored for this session.
                    if model.vision {
                        match self.inner.registry.lock_recover().image_store(&session_id) {
                            Ok(store) => policy = policy.with_images(Arc::new(store)),
                            Err(error) => eprintln!(
                                "xshelld warning: session {session_id}: cannot open its image \
store, so images are sent as placeholders: {error:#}"
                            ),
                        }
                    }
                    let outcome = run_agent_turn_with_budget(
                        agent.as_mut(),
                        &mut snapshot.history,
                        message,
                        &snapshot.descriptor.cwd,
                        &policy,
                        &budget,
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
                    let shell_result = run_direct_shell_streaming_cancellable(
                        &command,
                        &snapshot.descriptor.cwd,
                        cancellation.clone(),
                        |stream, text| {
                            execution.append(
                                &turn_id,
                                SessionEventKind::ShellOutput {
                                    stream: stream.into(),
                                    text,
                                },
                            )
                        },
                    )
                    .await;
                    let result = match shell_result {
                        Ok(result) => result,
                        Err(error) => {
                            audit.append(AuditEvent::ShellFinished {
                                command: command.clone(),
                                outcome: format!("error: {error:#}"),
                                cwd: snapshot.descriptor.cwd.display().to_string(),
                            })?;
                            return Err(error);
                        }
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
        let persistence = self.inner.registry.lock_recover().update_execution_state(
            &session_id,
            snapshot.descriptor.cwd,
            snapshot.history,
        );
        let terminal = if let Err(error) = persistence {
            SessionEventKind::TurnFailed {
                message: format!("cannot persist terminal turn state: {error:#}"),
                reason: None,
            }
        } else if let Some(error) = audit_failure {
            SessionEventKind::TurnFailed {
                message: format!("{error:#}"),
                reason: Some(TurnFailureReason::AuditUnavailable),
            }
        } else if cancellation.is_cancelled() {
            SessionEventKind::TurnCancelled
        } else if let Err(error) = result {
            let budget = error
                .chain()
                .any(|cause| cause.downcast_ref::<BudgetExhausted>().is_some());
            SessionEventKind::TurnFailed {
                message: format!("{error:#}"),
                reason: budget.then_some(TurnFailureReason::BudgetExhausted),
            }
        } else {
            SessionEventKind::TurnCompleted
        };
        if agent_turn {
            let usage = budget.usage();
            let audit_ref = record_turn_finished(&audit, &turn_id, &terminal, &usage).map(|last| {
                TurnAuditRef {
                    first_sequence: first_record
                        .filter(|first| first.audit_session_id == last.audit_session_id)
                        .map(|first| first.sequence),
                    audit_session_id: last.audit_session_id,
                    last_sequence: last.sequence,
                    record_hash: last.record_hash,
                }
            });
            execution.append(
                &turn_id,
                SessionEventKind::TurnSummary {
                    usage: Some(usage),
                    audit: audit_ref,
                },
            );
        }
        execution.conclude(&turn_id, terminal);
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
        append_locked(&mut state, turn_id, event);
        self.changed.notify_all();
    }

    /// Record a turn's terminal event and release the turn in one step, so a
    /// client that sees the event can immediately start the next operation.
    fn conclude(&self, turn_id: &str, event: SessionEventKind) {
        let mut state = self.state.lock_recover();
        append_locked(&mut state, turn_id, event);
        finish_locked(&mut state, turn_id);
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
        finish_locked(&mut state, turn_id);
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
        self.audit_execution(&event);
        // A subagent's approval is registered under its scoped call id, the
        // same id the client replies with.
        if let ExecutionEvent::ApprovalRequested { call, .. } = event.innermost() {
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

    async fn approve(&mut self, call: &ToolCall, _reason: GateReason) -> ApprovalAnswer {
        let key = (self.turn_id.clone(), call.id.clone());
        loop {
            if self.cancellation.is_cancelled() {
                return ApprovalAnswer::cancelled();
            }
            // Register for the wake-up before checking, so a reply that
            // arrives between the check and the await is not missed.
            let notified = self.execution.approvals_ready.notified();
            if let Some(answer) = self.execution.state.lock_recover().approvals.remove(&key) {
                return answer;
            }
            tokio::select! {
                () = notified => {}
                () = self.cancellation.wait() => {}
            }
        }
    }
}

impl DaemonObserver {
    /// Record `event` at the execution boundary. A subagent's events are
    /// recorded like the parent's, between its `SubagentStarted` and
    /// `SubagentFinished` records.
    fn audit_execution(&mut self, event: &ExecutionEvent) {
        match event {
            ExecutionEvent::TextDelta { .. } | ExecutionEvent::ApprovalRequested { .. } => {}
            ExecutionEvent::AgentResponse {
                content,
                tool_call_count,
                partial,
                reasoning,
                finish_reason,
            } => self.audit(AuditEvent::AgentResponse {
                content: content.clone(),
                tool_call_count: *tool_call_count,
                partial: *partial,
                reasoning: reasoning.clone(),
                finish_reason: finish_reason.clone(),
            }),
            ExecutionEvent::AutoContinued { reason, prompt } => {
                self.audit(AuditEvent::AutoContinued {
                    reason: reason.clone(),
                    prompt: prompt.clone(),
                })
            }
            ExecutionEvent::ToolRequested { call } => self.audit(AuditEvent::ToolRequested {
                call_id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            }),
            ExecutionEvent::ToolDecision {
                call_id,
                decision,
                basis,
                approval,
            } => self.audit(crate::audit_tool_decision(
                call_id,
                match decision {
                    ApprovalDecision::Approve => "approve",
                    ApprovalDecision::Deny => "deny",
                    ApprovalDecision::AbortTurn => "abort_turn",
                },
                basis.as_ref(),
                approval.as_ref(),
            )),
            ExecutionEvent::ToolSkipped { call_id, .. } => self.audit(crate::audit_tool_decision(
                call_id,
                "skipped_after_abort",
                None,
                None,
            )),
            ExecutionEvent::ToolResult {
                call_id,
                name,
                result,
                images,
            } => self.audit(AuditEvent::ToolResult {
                call_id: call_id.clone(),
                name: name.clone(),
                result: result.clone(),
                images: images.iter().map(crate::audit_image).collect(),
            }),
            ExecutionEvent::TurnAborted => {}
            ExecutionEvent::HistoryCompacted { report } => {
                self.audit(AuditEvent::HistoryCompacted {
                    compactor: report.compactor.clone(),
                    messages_before: report.messages_before,
                    messages_after: report.messages_after,
                    bytes_before: report.bytes_before,
                    bytes_after: report.bytes_after,
                    turns_removed: report.turns_removed,
                })
            }
            ExecutionEvent::SubagentStarted { agent, task, tools } => {
                self.audit(AuditEvent::SubagentStarted {
                    agent: agent.clone(),
                    task: task.clone(),
                    tools: tools.clone(),
                })
            }
            ExecutionEvent::Subagent { event, .. } => self.audit_execution(event),
            ExecutionEvent::SubagentFinished {
                agent,
                outcome,
                message,
            } => self.audit(AuditEvent::SubagentFinished {
                agent: agent.clone(),
                outcome: outcome.clone(),
                message: message.clone(),
            }),
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

fn append_locked(state: &mut ExecutionState, turn_id: &str, event: SessionEventKind) {
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
            state.event_bytes = state
                .event_bytes
                .saturating_sub(serde_json::to_vec(&removed).map_or(0, |encoded| encoded.len()));
        }
    }
}

fn finish_locked(state: &mut ExecutionState, turn_id: &str) {
    if state.active.as_ref().map(|turn| turn.id.as_str()) == Some(turn_id) {
        state.active = None;
    }
    state
        .pending_approvals
        .retain(|(pending_turn, _)| pending_turn != turn_id);
    state
        .approvals
        .retain(|(pending_turn, _), _| pending_turn != turn_id);
}

/// Append an agent turn's `turn_finished` audit record and return where it
/// landed. The turn's effects have already happened, so a failure here does
/// not change its outcome: the summary simply carries no audit reference,
/// which a supervisor reads as "not verifiable".
fn record_turn_finished(
    audit: &SessionAuditHandle,
    turn_id: &str,
    terminal: &SessionEventKind,
    usage: &TurnUsage,
) -> Option<AuditReceipt> {
    let (outcome, reason) = match terminal {
        SessionEventKind::TurnCompleted => ("completed", None),
        SessionEventKind::TurnCancelled => ("cancelled", None),
        SessionEventKind::TurnFailed { reason, .. } => ("failed", *reason),
        _ => ("failed", None),
    };
    let reason = reason.map(|reason| match reason {
        TurnFailureReason::BudgetExhausted => "budget_exhausted".to_owned(),
        TurnFailureReason::AuditUnavailable => "audit_unavailable".to_owned(),
    });
    let usage = serde_json::to_value(usage).ok().map(Box::new);
    match audit.append_with_receipt(AuditEvent::TurnFinished {
        turn_id: turn_id.to_owned(),
        outcome: outcome.to_owned(),
        reason,
        usage,
    }) {
        Ok(receipt) => receipt,
        Err(error) => {
            eprintln!("xshelld audit warning: cannot record the end of turn {turn_id}: {error:#}");
            None
        }
    }
}

/// Check a client's account of who answered an approval prompt before the
/// audit trail records it.
///
/// - `cancelled` is the daemon's own record of a prompt ended by
///   cancellation; a client cannot claim it.
/// - `no_approver` means nobody could answer and the client denied the
///   call, so it cannot come with an approval.
/// - The detail code is re-validated: deserialization does not apply
///   `with_detail`'s check.
fn validated_source(
    decision: ApprovalDecision,
    source: Option<ApprovalSource>,
) -> Result<ApprovalSource> {
    let Some(source) = source else {
        return Ok(ApprovalSource::new(Approver::Unreported));
    };
    match (source.approver, decision) {
        (Approver::Cancelled, _) => {
            bail!("a client cannot report a prompt as cancelled; cancel the turn instead")
        }
        (Approver::NoApprover, ApprovalDecision::Approve) => {
            bail!("an approval cannot come from no approver")
        }
        _ => {}
    }
    let detail = source.detail.unwrap_or_default();
    Ok(ApprovalSource::new(source.approver).with_detail(detail))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(approver: Approver, detail: Option<&str>) -> Option<ApprovalSource> {
        Some(ApprovalSource {
            approver,
            detail: detail.map(str::to_owned),
        })
    }

    #[test]
    fn turn_slots_stop_at_the_limit_and_are_given_back_when_dropped() {
        let running = Arc::new(AtomicUsize::new(0));
        let first = TurnSlot::acquire(&running, 2).unwrap();
        let second = TurnSlot::acquire(&running, 2).unwrap();
        assert!(TurnSlot::acquire(&running, 2).is_none());
        drop(first);
        let third = TurnSlot::acquire(&running, 2).unwrap();
        drop(second);
        drop(third);
        assert_eq!(running.load(Ordering::Acquire), 0);
    }

    #[test]
    fn clients_cannot_claim_a_cancelled_prompt() {
        for decision in [
            ApprovalDecision::Approve,
            ApprovalDecision::Deny,
            ApprovalDecision::AbortTurn,
        ] {
            assert!(validated_source(decision, source(Approver::Cancelled, None)).is_err());
        }
    }

    #[test]
    fn no_approver_only_denies() {
        assert!(
            validated_source(
                ApprovalDecision::Approve,
                source(Approver::NoApprover, Some("no_approval_channel"))
            )
            .is_err()
        );
        for decision in [ApprovalDecision::Deny, ApprovalDecision::AbortTurn] {
            let accepted = validated_source(
                decision,
                source(Approver::NoApprover, Some("no_approval_channel")),
            )
            .unwrap();
            assert_eq!(accepted.approver, Approver::NoApprover);
            assert_eq!(accepted.detail.as_deref(), Some("no_approval_channel"));
        }
    }

    #[test]
    fn reported_sources_are_kept_and_details_revalidated() {
        assert_eq!(
            validated_source(ApprovalDecision::Approve, None)
                .unwrap()
                .approver,
            Approver::Unreported
        );
        let accepted = validated_source(
            ApprovalDecision::Approve,
            source(Approver::ApprovalChannel, Some("Not A Code!")),
        )
        .unwrap();
        assert_eq!(accepted.approver, Approver::ApprovalChannel);
        assert_eq!(accepted.detail, None);
    }
}

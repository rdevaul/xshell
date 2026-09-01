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
use xshell_core::ToolCall;
use xshell_execution::{
    AdapterConfig, ApprovalDecision, ApprovalPolicy, CancellationFlag, ExecutionEvent,
    TurnObserver, build_adapter, run_agent_turn, run_direct_shell_streaming,
};

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
}

struct SessionExecution {
    state: Mutex<ExecutionState>,
    changed: Condvar,
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
        Self {
            inner: Arc::new(CoordinatorInner {
                registry,
                sessions: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn submit(
        &self,
        session_id: &str,
        input: TurnInput,
        approval: ApprovalPolicy,
    ) -> Result<String> {
        let snapshot = self
            .inner
            .registry
            .lock()
            .expect("session registry poisoned")
            .snapshot(session_id)?;
        let execution = self.session(session_id);
        let turn_id = Uuid::new_v4().to_string();
        let cancellation = CancellationFlag::default();
        {
            let mut state = execution.state.lock().expect("execution state poisoned");
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
        let mut state = execution.state.lock().expect("execution state poisoned");
        if state.active.as_ref().map(|turn| turn.id.as_str()) != Some(reply.turn_id.as_str()) {
            bail!("turn is no longer active");
        }
        let key = (reply.turn_id, reply.call_id);
        if !state.pending_approvals.remove(&key) {
            bail!("tool call is not waiting for approval");
        }
        state.approvals.insert(key, reply.decision);
        execution.changed.notify_all();
        Ok(())
    }

    pub fn cancel(&self, session_id: &str, turn_id: &str) -> Result<()> {
        let execution = self.session(session_id);
        let state = execution.state.lock().expect("execution state poisoned");
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
        let execution = self
            .inner
            .sessions
            .lock()
            .expect("execution map poisoned")
            .remove(session_id);
        if let Some(execution) = execution
            && let Some(active) = &execution
                .state
                .lock()
                .expect("execution state poisoned")
                .active
        {
            active.cancellation.cancel();
            execution.changed.notify_all();
        }
    }

    pub fn active_turn(&self, session_id: &str) -> Option<String> {
        self.session(session_id)
            .state
            .lock()
            .expect("execution state poisoned")
            .active
            .as_ref()
            .map(|turn| turn.id.clone())
    }

    pub fn activity(&self, session_id: &str) -> SessionActivity {
        let execution = self.session(session_id);
        let state = execution.state.lock().expect("execution state poisoned");
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
            .lock()
            .expect("execution map poisoned")
            .entry(session_id.to_owned())
            .or_insert_with(|| Arc::new(SessionExecution::new()))
            .clone()
    }

    fn run_turn(
        &self,
        session_id: String,
        turn_id: String,
        input: TurnInput,
        approval: ApprovalPolicy,
        mut snapshot: crate::SessionSnapshot,
        cancellation: CancellationFlag,
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
                    run_agent_turn(
                        agent.as_mut(),
                        &mut snapshot.history,
                        message,
                        &snapshot.descriptor.cwd,
                        approval,
                        &mut observer,
                    )
                    .await
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
                        execution.append(
                            &turn_id,
                            SessionEventKind::WorkingDirectoryChanged { cwd: result.cwd },
                        );
                    }
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

        if cancellation.is_cancelled() {
            execution.append(&turn_id, SessionEventKind::TurnCancelled);
        } else if let Err(error) = result {
            execution.append(
                &turn_id,
                SessionEventKind::TurnFailed {
                    message: format!("{error:#}"),
                },
            );
        } else {
            let update = self
                .inner
                .registry
                .lock()
                .expect("session registry poisoned")
                .update_execution_state(&session_id, snapshot.descriptor.cwd, snapshot.history);
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
        }
    }

    fn append(&self, turn_id: &str, event: SessionEventKind) {
        let mut state = self.state.lock().expect("execution state poisoned");
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
        let mut state = self.state.lock().expect("execution state poisoned");
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
                .expect("execution state poisoned");
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
        let mut state = self.state.lock().expect("execution state poisoned");
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
}

#[async_trait]
impl TurnObserver for DaemonObserver {
    fn emit(&mut self, event: ExecutionEvent) {
        if let ExecutionEvent::ApprovalRequested { call } = &event {
            self.execution
                .state
                .lock()
                .expect("execution state poisoned")
                .pending_approvals
                .insert((self.turn_id.clone(), call.id.clone()));
        }
        self.execution
            .append(&self.turn_id, SessionEventKind::Execution { event });
    }

    fn cancellation(&self) -> CancellationFlag {
        self.cancellation.clone()
    }

    async fn approve(&mut self, call: &ToolCall) -> ApprovalDecision {
        let key = (self.turn_id.clone(), call.id.clone());
        loop {
            if self.cancellation.is_cancelled() {
                return ApprovalDecision::AbortTurn;
            }
            if let Some(decision) = self
                .execution
                .state
                .lock()
                .expect("execution state poisoned")
                .approvals
                .remove(&key)
            {
                return decision;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
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

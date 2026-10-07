use crate::config::ActiveModel;
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use xshell_core::ChatMessage;
use xshell_execution::{ApprovalDecision, ApprovalPolicy};
use xshell_session::{
    AgentTurnPhase, EventBatch, PersistenceMode, PtySize, PtyStreamClient, PtyTicket,
    RemoteBootstrapAction, RemoteRepair, SessionActivity, SessionClient, SessionConfig,
    SessionCreation, SessionDescriptor, SessionSnapshot, SessionStatus, TurnInput, ViewResource,
    Visibility,
};
use xshell_view::sanitize_terminal_text;

struct HostConnection {
    client: SessionClient,
    endpoint: ConnectionEndpoint,
}

enum ConnectionEndpoint {
    Local(PathBuf),
    Ssh(String),
}

pub struct SessionRuntime {
    connection: Option<HostConnection>,
    parked_connections: HashMap<String, HostConnection>,
    display_host_aliases: HashMap<String, String>,
    active: Option<SessionDescriptor>,
    navigation_history: Vec<String>,
    event_cursors: HashMap<String, u64>,
    pty_cursors: HashMap<String, u64>,
}

impl SessionRuntime {
    pub fn start(
        config: &SessionConfig,
        requested_name: Option<&str>,
        model: &ActiveModel,
        cwd: &Path,
        history: &[ChatMessage],
    ) -> Result<(Self, Option<SessionSnapshot>)> {
        if !config.enabled {
            return Ok((Self::disabled(), None));
        }
        let socket = resolve_socket(config)?;
        let mut client = match SessionClient::connect(&socket, env!("CARGO_PKG_VERSION")) {
            Ok(client) => client,
            Err(error) if !config.required => {
                eprintln!("xshell: session service unavailable; continuing locally: {error:#}");
                return Ok((Self::disabled(), None));
            }
            Err(error) => return Err(error),
        };
        let name = requested_name.unwrap_or(&config.default_session);
        let exists = client
            .list()?
            .iter()
            .any(|session| session.name == name || session.id == name);
        let snapshot = if exists {
            client.attach(name.to_owned())?
        } else {
            client.create(SessionCreation {
                name: name.to_owned(),
                model: model.to_session_binding(),
                cwd: cwd.to_owned(),
                persistence: PersistenceMode::Daemon,
                visibility: Visibility::Fabric,
                history: history.to_vec(),
            })?
        };
        let active = Some(snapshot.descriptor.clone());
        let cursor = initial_event_cursor(&mut client, &snapshot.descriptor.id)?;
        let event_cursors = HashMap::from([(snapshot.descriptor.id.clone(), cursor)]);
        let mut runtime = Self {
            connection: Some(HostConnection {
                client,
                endpoint: ConnectionEndpoint::Local(socket),
            }),
            parked_connections: HashMap::new(),
            display_host_aliases: HashMap::new(),
            active,
            navigation_history: Vec::new(),
            event_cursors,
            pty_cursors: HashMap::new(),
        };
        runtime.rebuild_display_host_aliases();
        Ok((runtime, Some(snapshot)))
    }

    pub fn disabled() -> Self {
        Self {
            connection: None,
            parked_connections: HashMap::new(),
            display_host_aliases: HashMap::new(),
            active: None,
            navigation_history: Vec::new(),
            event_cursors: HashMap::new(),
            pty_cursors: HashMap::new(),
        }
    }

    pub fn active(&self) -> Option<&SessionDescriptor> {
        self.active.as_ref()
    }

    pub fn display_host_alias<'a>(&'a self, session: &'a SessionDescriptor) -> &'a str {
        self.display_host_aliases
            .get(&session.host_id)
            .map(String::as_str)
            .unwrap_or(&session.host_alias)
    }

    pub fn service_label(&self) -> Option<String> {
        match &self.connection.as_ref()?.endpoint {
            ConnectionEndpoint::Local(socket) => Some(socket.display().to_string()),
            ConnectionEndpoint::Ssh(destination) => Some(format!("ssh://{destination}")),
        }
    }

    pub fn enabled(&self) -> bool {
        self.connection.is_some()
    }

    pub fn remote_completion_client(&self) -> Result<Option<(SessionClient, String)>> {
        let Some(connection) = &self.connection else {
            return Ok(None);
        };
        let ConnectionEndpoint::Ssh(destination) = &connection.endpoint else {
            return Ok(None);
        };
        let session_id = self
            .active
            .as_ref()
            .map(|session| session.id.clone())
            .context("there is no active remote session")?;
        let client = SessionClient::connect_ssh(destination, env!("CARGO_PKG_VERSION"))?;
        Ok(Some((client, session_id)))
    }

    pub fn list(&mut self) -> Result<Vec<SessionDescriptor>> {
        self.rebuild_display_host_aliases();
        let mut sessions = Vec::new();
        let active_error = match self.client_mut()?.list() {
            Ok(catalog) => {
                sessions.extend(catalog);
                None
            }
            Err(error) => Some(error),
        };
        let host_ids = self.parked_connections.keys().cloned().collect::<Vec<_>>();
        for host_id in host_ids {
            let result = self
                .parked_connections
                .get_mut(&host_id)
                .expect("parked host exists")
                .client
                .list();
            match result {
                Ok(catalog) => sessions.extend(catalog),
                Err(error) => {
                    eprintln!("xshell: dropping unavailable host connection: {error:#}");
                    self.parked_connections.remove(&host_id);
                    self.rebuild_display_host_aliases();
                }
            }
        }
        if let Some(error) = active_error {
            if sessions.is_empty() {
                return Err(error);
            }
            eprintln!("xshell: active host connection is unavailable: {error:#}");
        }
        for session in &mut sessions {
            if let Some(alias) = self.display_host_aliases.get(&session.host_id) {
                session.host_alias.clone_from(alias);
            }
        }
        Ok(sessions)
    }

    pub fn session_names(&mut self) -> Result<Vec<String>> {
        if self.connection.is_none() {
            return Ok(Vec::new());
        }
        let local_host_id = self.local_host_id().map(str::to_owned);
        let mut names = Vec::new();
        for session in self.list()? {
            names.push(format!("{}:{}", session.host_alias, session.name));
            if local_host_id.as_deref() == Some(session.host_id.as_str()) {
                names.push(format!("local:{}", session.name));
            }
        }
        Ok(names)
    }

    pub fn connect_ssh(
        &mut self,
        destination: &str,
        requested_session: Option<&str>,
        default_session: &str,
        model: &ActiveModel,
        system_prompt: &str,
    ) -> Result<SessionSnapshot> {
        match SessionClient::probe_ssh(destination) {
            Ok(probe) => prepare_remote_host(destination, probe.required_action())?,
            Err(error) => eprintln!(
                "xshell: remote capability probe unavailable; trying the legacy direct connection: {error:#}"
            ),
        }
        let mut client = SessionClient::connect_ssh(destination, env!("CARGO_PKG_VERSION"))?;
        let host_id = client.host_id().to_owned();
        if self
            .connection
            .as_ref()
            .is_some_and(|connection| connection.client.host_id() == host_id)
            || self.parked_connections.contains_key(&host_id)
        {
            return Err(anyhow::anyhow!(
                "host {} is already connected; use //switch {}:SESSION",
                client.host_alias(),
                client.host_alias()
            ));
        }

        let session_name = requested_session.unwrap_or(default_session);
        let exists = client
            .list()?
            .iter()
            .any(|session| session.name == session_name || session.id == session_name);
        let snapshot = if exists {
            client.attach(session_name.to_owned())?
        } else {
            client.create(SessionCreation {
                name: session_name.to_owned(),
                model: model.to_session_binding(),
                cwd: PathBuf::from("~"),
                persistence: PersistenceMode::Daemon,
                visibility: Visibility::Fabric,
                history: vec![ChatMessage::system(system_prompt)],
            })?
        };

        if let Some(previous) = self.active.as_ref().map(|session| session.id.clone()) {
            self.navigation_history.push(previous);
        }
        if let Some(connection) = self.connection.take() {
            self.parked_connections
                .insert(connection.client.host_id().to_owned(), connection);
        }
        self.connection = Some(HostConnection {
            client,
            endpoint: ConnectionEndpoint::Ssh(destination.to_owned()),
        });
        self.rebuild_display_host_aliases();
        self.active = Some(snapshot.descriptor.clone());
        self.ensure_event_cursor(&snapshot.descriptor.id)?;
        Ok(snapshot)
    }

    pub fn sync(&mut self, model: &ActiveModel, cwd: &Path, history: &[ChatMessage]) -> Result<()> {
        let Some((session_id, interactive_process)) = self.active.as_ref().map(|session| {
            (
                session.id.clone(),
                session.activity.is_interactive_process(),
            )
        }) else {
            return Ok(());
        };
        let model = model.to_session_binding();
        if interactive_process {
            let snapshot = self.client_mut()?.snapshot(session_id.clone())?;
            if snapshot.descriptor.activity.is_interactive_process() {
                let unchanged = snapshot.descriptor.model == model
                    && snapshot.descriptor.cwd == cwd
                    && snapshot.history == history;
                self.active = Some(snapshot.descriptor);
                if !unchanged {
                    bail!(
                        "cannot change the model, working directory, or conversation while the \
session's interactive process is running; stop it first"
                    );
                }
                return Ok(());
            }
            // A process may have exited while its controller was detached.
            // Refresh the cached activity and continue with the ordinary update.
            self.active = Some(snapshot.descriptor);
        }
        let descriptor =
            self.client_mut()?
                .update(session_id, model, cwd.to_owned(), history.to_vec())?;
        self.active = Some(descriptor);
        Ok(())
    }

    pub fn switch(&mut self, selector: &str) -> Result<SessionSnapshot> {
        let previous = self.active.as_ref().map(|session| session.id.clone());
        let (host_id, session_id) = self.resolve_target(selector)?;
        let active_host_id = self.client_mut()?.host_id().to_owned();
        let snapshot = if host_id == active_host_id {
            self.client_mut()?.switch(session_id)?
        } else {
            let mut target = self
                .parked_connections
                .remove(&host_id)
                .context("session host connection disappeared")?;
            let snapshot = match target.client.switch(session_id) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    self.parked_connections.insert(host_id, target);
                    return Err(error);
                }
            };
            let current = self
                .connection
                .replace(target)
                .context("there is no active host connection")?;
            self.parked_connections
                .insert(current.client.host_id().to_owned(), current);
            snapshot
        };
        if let Some(previous) = previous
            && previous != snapshot.descriptor.id
        {
            self.navigation_history.push(previous);
        }
        self.active = Some(snapshot.descriptor.clone());
        self.ensure_event_cursor(&snapshot.descriptor.id)?;
        Ok(snapshot)
    }

    pub fn create(
        &mut self,
        name: String,
        model: &ActiveModel,
        cwd: &Path,
        history: Vec<ChatMessage>,
        persistence: PersistenceMode,
        visibility: Visibility,
    ) -> Result<SessionSnapshot> {
        let previous = self.active.as_ref().map(|session| session.id.clone());
        let snapshot = self.client_mut()?.create(SessionCreation {
            name,
            model: model.to_session_binding(),
            cwd: cwd.to_owned(),
            persistence,
            visibility,
            history,
        })?;
        if let Some(previous) = previous {
            self.navigation_history.push(previous);
        }
        self.active = Some(snapshot.descriptor.clone());
        self.ensure_event_cursor(&snapshot.descriptor.id)?;
        Ok(snapshot)
    }

    /// Create a fresh sibling of the active session on the same host. This is
    /// used by the raw-mode session picker, where the normal REPL arguments are
    /// deliberately unavailable.
    pub fn create_sibling(&mut self, name: String) -> Result<SessionSnapshot> {
        let current = self
            .active
            .clone()
            .context("there is no active session to copy")?;
        let snapshot = self.client_mut()?.snapshot(current.id.clone())?;
        let history = snapshot.history.into_iter().take(1).collect();
        let previous = current.id;
        let created = self.client_mut()?.create(SessionCreation {
            name,
            model: current.model,
            cwd: current.cwd,
            history,
            persistence: PersistenceMode::Daemon,
            visibility: Visibility::Fabric,
        })?;
        self.navigation_history.push(previous);
        self.active = Some(created.descriptor.clone());
        self.ensure_event_cursor(&created.descriptor.id)?;
        Ok(created)
    }

    pub fn detach(&mut self) -> Result<Option<String>> {
        let detached = self.client_mut()?.detach()?;
        self.active = None;
        Ok(detached)
    }

    pub fn submit(&mut self, input: TurnInput, approval: ApprovalPolicy) -> Result<String> {
        let session_id = self.active_session_id()?;
        let turn_id = self
            .client_mut()?
            .submit(session_id.clone(), input, approval)?;
        if let Some(active) = &mut self.active {
            active.activity = SessionActivity::AgentTurn {
                turn_id: turn_id.clone(),
                phase: AgentTurnPhase::Running,
            };
        }
        Ok(turn_id)
    }

    pub fn view_source(&mut self, path: PathBuf) -> Result<ViewResource> {
        let session_id = self.active_session_id()?;
        self.client_mut()?.view_source(session_id, path)
    }

    pub fn pty_start(
        &mut self,
        command: String,
        size: PtySize,
        terminal_type: Option<String>,
    ) -> Result<PtyTicket> {
        let session_id = self.active_session_id()?;
        let ticket =
            self.client_mut()?
                .pty_start(session_id, command.clone(), size, terminal_type)?;
        if let Some(active) = &mut self.active {
            active.activity = SessionActivity::InteractiveProcess {
                command: command.clone(),
                attached: false,
            };
        }
        Ok(ticket)
    }

    pub fn pty_start_stream(
        &mut self,
        command: String,
        size: PtySize,
        terminal_type: Option<String>,
    ) -> Result<PtyStreamClient> {
        let ticket = self.pty_start(command, size, terminal_type)?;
        match self.open_pty_ticket(&ticket) {
            Ok(stream) => Ok(stream),
            Err(error) => {
                let _ = self.pty_close_current();
                Err(error)
            }
        }
    }

    pub fn pty_attach_stream(&mut self) -> Result<PtyStreamClient> {
        self.pty_attach_stream_if_present()?
            .context("active session has no terminal job")
    }

    pub fn active_interactive_running(&mut self) -> Result<bool> {
        let session_id = self.active_session_id()?;
        let snapshot = self.client_mut()?.snapshot(session_id)?;
        let running = snapshot.descriptor.activity.is_interactive_process();
        self.active = Some(snapshot.descriptor);
        Ok(running)
    }

    pub fn pty_attach_stream_if_present(&mut self) -> Result<Option<PtyStreamClient>> {
        let session_id = self.active_session_id()?;
        let snapshot = self.client_mut()?.snapshot(session_id.clone())?;
        if !snapshot.descriptor.activity.is_interactive_process() {
            self.active = Some(snapshot.descriptor);
            return Ok(None);
        }
        self.active = Some(snapshot.descriptor);
        let after_offset = self.pty_cursors.get(&session_id).copied();
        let ticket = self
            .client_mut()?
            .pty_attach(session_id.clone(), after_offset)?;
        let stream = self.open_pty_ticket(&ticket)?;
        Ok(Some(stream))
    }

    pub fn remember_pty_cursor(&mut self, cursor: u64) {
        if let Some(session) = &self.active {
            self.pty_cursors.insert(session.id.clone(), cursor);
        }
    }

    pub fn session_targets(&mut self) -> Result<Vec<SessionDescriptor>> {
        let mut sessions = self.list()?;
        sessions.sort_by(|left, right| {
            (&left.host_alias, &left.name).cmp(&(&right.host_alias, &right.name))
        });
        Ok(sessions)
    }

    pub fn previous_session_id(&self) -> Option<&str> {
        self.navigation_history.last().map(String::as_str)
    }

    pub fn pty_close_current(&mut self) -> Result<()> {
        let session_id = self.active_session_id()?;
        self.client_mut()?.pty_close(session_id.clone())?;
        self.pty_cursors.remove(&session_id);
        if let Some(active) = &mut self.active {
            active.activity = SessionActivity::Idle;
        }
        Ok(())
    }

    pub fn events(&mut self, wait_ms: u64) -> Result<EventBatch> {
        let session_id = self.active_session_id()?;
        let after_sequence = self.event_cursors.get(&session_id).copied().unwrap_or(0);
        self.client_mut()?
            .events(session_id, after_sequence, wait_ms)
    }

    /// Commit one event after the controller has handled it. In particular an
    /// approval request is left unacknowledged when focus moves elsewhere, so
    /// reattaching replays the request instead of stranding the daemon turn.
    pub fn acknowledge_event(&mut self, sequence: u64) -> Result<()> {
        let session_id = self.active_session_id()?;
        let cursor = self.event_cursors.entry(session_id).or_insert(0);
        *cursor = (*cursor).max(sequence);
        Ok(())
    }

    pub fn approve(
        &mut self,
        turn_id: String,
        call_id: String,
        decision: ApprovalDecision,
    ) -> Result<()> {
        let session_id = self.active_session_id()?;
        self.client_mut()?
            .approve(session_id, turn_id, call_id, decision)
    }

    pub fn refresh_snapshot(&mut self) -> Result<SessionSnapshot> {
        let session_id = self.active_session_id()?;
        let snapshot = self.client_mut()?.snapshot(session_id)?;
        self.active = Some(snapshot.descriptor.clone());
        Ok(snapshot)
    }

    pub fn active_turn_id(&self) -> Option<&str> {
        match &self.active.as_ref()?.activity {
            SessionActivity::AgentTurn { turn_id, .. } => Some(turn_id),
            _ => None,
        }
    }

    pub fn mark_turn_finished(&mut self) {
        if let Some(active) = &mut self.active {
            active.activity = SessionActivity::Idle;
        }
    }

    pub fn stop_current_activity(&mut self) -> Result<&'static str> {
        let snapshot = self.refresh_snapshot()?;
        match snapshot.descriptor.activity {
            SessionActivity::Idle => bail!("the active session has no running activity"),
            SessionActivity::InteractiveProcess { .. } => {
                self.pty_close_current()?;
                Ok("interactive process stopped")
            }
            SessionActivity::AgentTurn { turn_id, .. } => {
                let session_id = snapshot.descriptor.id;
                self.client_mut()?.cancel(session_id.clone(), turn_id)?;
                if let Some(active) = &mut self.active {
                    active.activity = SessionActivity::Idle;
                }
                Ok("agent turn cancellation requested")
            }
        }
    }

    pub fn close_current_and_fallback(&mut self) -> Result<Option<SessionSnapshot>> {
        let current_id = self
            .active
            .as_ref()
            .map(|session| session.id.clone())
            .context("there is no active session to close")?;
        let catalog = self.list()?;
        self.client_mut()?.close(None)?;
        self.active = None;
        self.event_cursors.remove(&current_id);
        self.navigation_history
            .retain(|session_id| session_id != &current_id);

        let candidates = fallback_candidates(&self.navigation_history, catalog, &current_id);

        let mut fallback = None;
        for candidate in candidates {
            if let Ok(snapshot) = self.switch(&candidate) {
                self.navigation_history
                    .retain(|session_id| session_id != &snapshot.descriptor.id);
                self.active = Some(snapshot.descriptor.clone());
                fallback = Some(snapshot);
                break;
            }
        }
        if let Some(snapshot) = &fallback {
            self.ensure_event_cursor(&snapshot.descriptor.id)?;
        }
        Ok(fallback)
    }

    fn client_mut(&mut self) -> Result<&mut SessionClient> {
        self.connection
            .as_mut()
            .map(|connection| &mut connection.client)
            .context("session fabric is disabled; enable [session_fabric] and start xshelld")
    }

    fn open_pty_ticket(&self, ticket: &PtyTicket) -> Result<PtyStreamClient> {
        match &self
            .connection
            .as_ref()
            .context("session service is disabled")?
            .endpoint
        {
            ConnectionEndpoint::Local(socket) => {
                PtyStreamClient::connect_local(socket, &ticket.ticket, ticket.replay_from)
            }
            ConnectionEndpoint::Ssh(destination) => {
                PtyStreamClient::connect_ssh(destination, &ticket.ticket, ticket.replay_from)
            }
        }
    }

    fn resolve_target(&mut self, selector: &str) -> Result<(String, String)> {
        let local_selector = selector.strip_prefix("local:");
        let local_host_id = if local_selector.is_some() {
            Some(
                self.local_host_id()
                    .context("no local xshelld connection is available")?
                    .to_owned(),
            )
        } else {
            None
        };
        let matches = self
            .list()?
            .into_iter()
            .filter(|session| {
                if let (Some(name), Some(host_id)) = (local_selector, local_host_id.as_deref()) {
                    session.host_id == host_id && session.name == name
                } else {
                    session.id == selector
                        || session.name == selector
                        || format!("{}:{}", session.host_alias, session.name) == selector
                        || format!("{}/{}:{}", session.host_alias, session.user, session.name)
                            == selector
                }
            })
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [session] => Ok((session.host_id.clone(), session.id.clone())),
            [] => Err(anyhow::anyhow!("unknown session {selector:?}")),
            _ => Err(anyhow::anyhow!(
                "session name {selector:?} is ambiguous; use HOST:SESSION"
            )),
        }
    }

    fn local_host_id(&self) -> Option<&str> {
        self.connection
            .as_ref()
            .filter(|connection| matches!(&connection.endpoint, ConnectionEndpoint::Local(_)))
            .map(|connection| connection.client.host_id())
            .or_else(|| {
                self.parked_connections
                    .values()
                    .find(|connection| matches!(&connection.endpoint, ConnectionEndpoint::Local(_)))
                    .map(|connection| connection.client.host_id())
            })
    }

    fn active_session_id(&self) -> Result<String> {
        self.active
            .as_ref()
            .map(|session| session.id.clone())
            .context("there is no active session")
    }

    fn rebuild_display_host_aliases(&mut self) {
        let mut hosts = Vec::new();
        if let Some(connection) = &self.connection {
            hosts.push((
                connection.client.host_id().to_owned(),
                connection.client.host_alias().to_owned(),
            ));
        }
        hosts.extend(self.parked_connections.values().map(|connection| {
            (
                connection.client.host_id().to_owned(),
                connection.client.host_alias().to_owned(),
            )
        }));
        self.display_host_aliases = disambiguate_host_aliases(&hosts);
    }

    fn ensure_event_cursor(&mut self, session_id: &str) -> Result<()> {
        if self.event_cursors.contains_key(session_id) {
            return Ok(());
        }
        let cursor = initial_event_cursor(self.client_mut()?, session_id)?;
        self.event_cursors.insert(session_id.to_owned(), cursor);
        Ok(())
    }
}

/// Bring a remote host to a connectable state, repairing it once if the user
/// authorizes the repair.
///
/// A repair runs the remote `xshelld`'s own `service` verb; it installs
/// nothing. The host is re-probed afterwards rather than assumed fixed, because
/// a service manager reporting success is not the same fact as a daemon
/// answering the protocol.
fn prepare_remote_host(destination: &str, action: RemoteBootstrapAction) -> Result<()> {
    let Some(repair) = RemoteRepair::for_action(&action) else {
        return require_connectable_remote(action);
    };
    describe_repair(destination, &action, repair);
    if !confirm_remote_repair(destination, repair)? {
        // Declining is an ordinary outcome, so report the state the user is
        // choosing to leave in place rather than a bare refusal.
        return require_connectable_remote(action);
    }

    let output = SessionClient::repair_ssh(destination, repair)?;
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        println!("  {}", sanitize_terminal_text(line));
    }

    let action = SessionClient::probe_ssh(destination)
        .with_context(|| {
            format!(
                "cannot re-probe {destination:?} after `xshelld service {}`",
                repair.verb()
            )
        })?
        .required_action();
    if action == RemoteBootstrapAction::Connect {
        println!("xshell: {destination} is ready");
        return Ok(());
    }
    // One attempt only: repeating a repair that did not take would just wait
    // out the same timeout again.
    require_connectable_remote(action).with_context(|| {
        format!(
            "`xshelld service {}` on {destination:?} did not make the host connectable",
            repair.verb()
        )
    })
}

/// State the repair and its blast radius before asking for it.
fn describe_repair(destination: &str, action: &RemoteBootstrapAction, repair: RemoteRepair) {
    match action {
        RemoteBootstrapAction::Start => {
            println!("xshell: {destination} has xshelld installed but its daemon is not running");
        }
        RemoteBootstrapAction::Restart { reason } => {
            println!("xshell: {destination} needs its daemon restarted: {reason}");
            println!(
                "  a restart ends work that daemon owns, including sessions held by other \
                 controllers attached to {destination}"
            );
        }
        _ => {}
    }
    println!(
        "  proposed: ssh {destination} xshelld service {}",
        repair.verb()
    );
}

/// Ask before mutating another host.
///
/// `//connect` is user-initiated, but starting or restarting a service on a
/// different machine is still a remote effect with its own blast radius, and it
/// is deliberately not governed by the agent approval policy: `--approval off`
/// expresses trust in the model, not authority to restart another host's
/// daemon. A non-interactive controller cannot answer, so it is told what to
/// run instead of having the repair performed on its behalf.
fn confirm_remote_repair(destination: &str, repair: RemoteRepair) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!(
            "{destination:?} needs `xshelld service {}` but this controller is not interactive; \
             run it on that host, or connect from a terminal to authorize it",
            repair.verb()
        );
    }
    loop {
        print!(
            "Run `xshelld service {}` on {destination}? [y/N] ",
            repair.verb()
        );
        io::stdout()
            .flush()
            .context("could not flush repair prompt")?;
        let mut answer = String::new();
        if io::stdin()
            .read_line(&mut answer)
            .context("could not read repair approval")?
            == 0
        {
            return Ok(false);
        }
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "" | "n" | "no" => return Ok(false),
            _ => eprintln!("Please answer y (repair) or n (leave the host alone)."),
        }
    }
}

fn require_connectable_remote(action: RemoteBootstrapAction) -> Result<()> {
    match action {
        RemoteBootstrapAction::Connect => Ok(()),
        RemoteBootstrapAction::Install => bail!(
            "xshelld is not installed on the remote host; remote installation requires the signed-artifact bootstrap increment"
        ),
        RemoteBootstrapAction::Upgrade {
            binary_version,
            supported_protocol_version,
        } => bail!(
            "remote xshelld {binary_version} supports protocol {supported_protocol_version}, but this xshell requires protocol {}; remote upgrade requires the signed-artifact bootstrap increment",
            xshell_session::SESSION_PROTOCOL_VERSION
        ),
        RemoteBootstrapAction::Start => bail!(
            "the remote xshelld binary is compatible but its daemon is unavailable; run `xshelld service start` on that host, or reconnect and authorize the repair"
        ),
        RemoteBootstrapAction::Restart { reason } => {
            bail!(
                "the installed remote xshelld is compatible but its daemon must be restarted ({reason}); run `xshelld service restart` on that host, or reconnect and authorize the repair"
            )
        }
        RemoteBootstrapAction::Rejected { code, message } => {
            bail!("remote xshelld probe was rejected ({code}): {message}")
        }
    }
}

fn disambiguate_host_aliases(hosts: &[(String, String)]) -> HashMap<String, String> {
    let mut aliases: HashMap<&str, Vec<&str>> = HashMap::new();
    for (host_id, alias) in hosts {
        aliases.entry(alias).or_default().push(host_id);
    }

    let mut labels = HashMap::new();
    for (host_id, alias) in hosts {
        let colliding = &aliases[alias.as_str()];
        if colliding.len() == 1 {
            labels.insert(host_id.clone(), alias.clone());
            continue;
        }
        let prefix_length = (8..=host_id.len())
            .find(|length| {
                let prefix = &host_id[..*length];
                colliding
                    .iter()
                    .filter(|candidate| candidate.starts_with(prefix))
                    .count()
                    == 1
            })
            .unwrap_or(host_id.len());
        labels.insert(
            host_id.clone(),
            format!("{alias}#{}", &host_id[..prefix_length]),
        );
    }
    labels
}

fn initial_event_cursor(client: &mut SessionClient, session_id: &str) -> Result<u64> {
    let batch = client.events(session_id.to_owned(), 0, 0)?;
    let cursor = if batch.active_turn_id.is_some() {
        batch
            .events
            .first()
            .map(|event| event.sequence.saturating_sub(1))
            .unwrap_or_else(|| batch.next_sequence.saturating_sub(1))
    } else {
        batch.next_sequence.saturating_sub(1)
    };
    Ok(cursor)
}

fn fallback_candidates(
    navigation_history: &[String],
    mut catalog: Vec<SessionDescriptor>,
    current_id: &str,
) -> Vec<String> {
    let mut candidates = navigation_history
        .iter()
        .rev()
        .filter(|session_id| session_id.as_str() != current_id)
        .cloned()
        .collect::<Vec<_>>();
    catalog.sort_by_key(|session| std::cmp::Reverse(session.last_active_at_unix_ms));
    candidates.extend(
        catalog
            .into_iter()
            .filter(|session| session.id != current_id && session.status == SessionStatus::Detached)
            .map(|session| session.id),
    );
    let mut seen = HashSet::new();
    candidates.retain(|session_id| seen.insert(session_id.clone()));
    candidates
}

fn resolve_socket(config: &SessionConfig) -> Result<PathBuf> {
    config
        .resolved_socket()
        .context("HOME and XDG_STATE_HOME are not set; configure session_fabric.socket")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_fallback_prefers_reverse_navigation_history() {
        let history = vec!["first".into(), "second".into(), "first".into()];
        assert_eq!(
            fallback_candidates(&history, Vec::new(), "current"),
            vec!["first", "second"]
        );
    }

    #[test]
    fn duplicate_host_aliases_receive_stable_unique_labels() {
        let labels = disambiguate_host_aliases(&[
            ("12345678-aaaa".into(), "Mac.lan".into()),
            ("87654321-bbbb".into(), "Mac.lan".into()),
            ("abcdef00-cccc".into(), "jarvis".into()),
        ]);
        assert_eq!(labels["12345678-aaaa"], "Mac.lan#12345678");
        assert_eq!(labels["87654321-bbbb"], "Mac.lan#87654321");
        assert_eq!(labels["abcdef00-cccc"], "jarvis");
    }

    #[test]
    fn remote_preflight_only_connects_when_no_repair_is_needed() {
        assert!(require_connectable_remote(RemoteBootstrapAction::Connect).is_ok());
        assert!(
            require_connectable_remote(RemoteBootstrapAction::Install)
                .unwrap_err()
                .to_string()
                .contains("not installed")
        );
        assert!(
            require_connectable_remote(RemoteBootstrapAction::Start)
                .unwrap_err()
                .to_string()
                .contains("daemon is unavailable")
        );
        assert!(
            require_connectable_remote(RemoteBootstrapAction::Upgrade {
                binary_version: "0.1.0".into(),
                supported_protocol_version: 10,
            })
            .unwrap_err()
            .to_string()
            .contains("supports protocol 10")
        );
    }

    /// Only the two repairs that need no new bytes on the remote host are in
    /// scope. Install and upgrade place and execute binaries and wait on the
    /// deployment authorization contract, so they must not become repairable
    /// by accident when a new action variant is added.
    #[test]
    fn only_start_and_restart_are_repairable_without_installing() {
        assert_eq!(
            RemoteRepair::for_action(&RemoteBootstrapAction::Start),
            Some(RemoteRepair::Start)
        );
        assert_eq!(
            RemoteRepair::for_action(&RemoteBootstrapAction::Restart {
                reason: "old daemon".into()
            }),
            Some(RemoteRepair::Restart)
        );

        for action in [
            RemoteBootstrapAction::Connect,
            RemoteBootstrapAction::Install,
            RemoteBootstrapAction::Upgrade {
                binary_version: "0.1.0".into(),
                supported_protocol_version: 10,
            },
            RemoteBootstrapAction::Rejected {
                code: "handshake".into(),
                message: "no".into(),
            },
        ] {
            assert_eq!(
                RemoteRepair::for_action(&action),
                None,
                "{action:?} must not be repaired by a service verb"
            );
        }
    }

    /// The repair runs an `xshelld service` verb and nothing else, so a
    /// destination can never be interpreted as part of the remote command.
    #[test]
    fn repair_verbs_name_the_service_subcommand() {
        assert_eq!(RemoteRepair::Start.verb(), "start");
        assert_eq!(RemoteRepair::Restart.verb(), "restart");
    }

    /// Declining a repair must leave the user with the same actionable
    /// diagnosis they would get with repair unavailable, including how to fix
    /// the host by hand.
    #[test]
    fn declining_a_repair_still_explains_how_to_fix_the_host() {
        let start = require_connectable_remote(RemoteBootstrapAction::Start)
            .unwrap_err()
            .to_string();
        assert!(start.contains("xshelld service start"), "{start}");

        let restart = require_connectable_remote(RemoteBootstrapAction::Restart {
            reason: "running daemon speaks protocol 11".into(),
        })
        .unwrap_err()
        .to_string();
        assert!(restart.contains("xshelld service restart"), "{restart}");
        assert!(
            restart.contains("running daemon speaks protocol 11"),
            "the probe's reason must survive into the message: {restart}"
        );
    }
}

use crate::config::ActiveModel;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use xshell_core::ChatMessage;
use xshell_execution::{ApprovalDecision, ApprovalPolicy};
use xshell_session::{
    EventBatch, PersistenceMode, SessionClient, SessionConfig, SessionCreation, SessionDescriptor,
    SessionSnapshot, SessionStatus, TurnInput, ViewResource, Visibility,
};

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
    active: Option<SessionDescriptor>,
    navigation_history: Vec<String>,
    event_cursors: HashMap<String, u64>,
    active_turns: HashMap<String, Option<String>>,
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
        let (cursor, active_turn) = initial_event_cursor(&mut client, &snapshot.descriptor.id)?;
        let event_cursors = HashMap::from([(snapshot.descriptor.id.clone(), cursor)]);
        let active_turns = HashMap::from([(snapshot.descriptor.id.clone(), active_turn)]);
        Ok((
            Self {
                connection: Some(HostConnection {
                    client,
                    endpoint: ConnectionEndpoint::Local(socket),
                }),
                parked_connections: HashMap::new(),
                active,
                navigation_history: Vec::new(),
                event_cursors,
                active_turns,
            },
            Some(snapshot),
        ))
    }

    pub fn disabled() -> Self {
        Self {
            connection: None,
            parked_connections: HashMap::new(),
            active: None,
            navigation_history: Vec::new(),
            event_cursors: HashMap::new(),
            active_turns: HashMap::new(),
        }
    }

    pub fn active(&self) -> Option<&SessionDescriptor> {
        self.active.as_ref()
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
                }
            }
        }
        if let Some(error) = active_error {
            if sessions.is_empty() {
                return Err(error);
            }
            eprintln!("xshell: active host connection is unavailable: {error:#}");
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
        self.active = Some(snapshot.descriptor.clone());
        self.ensure_event_cursor(&snapshot.descriptor.id)?;
        Ok(snapshot)
    }

    pub fn sync(&mut self, model: &ActiveModel, cwd: &Path, history: &[ChatMessage]) -> Result<()> {
        let Some(session_id) = self.active.as_ref().map(|session| session.id.clone()) else {
            return Ok(());
        };
        let descriptor = self.client_mut()?.update(
            session_id,
            model.to_session_binding(),
            cwd.to_owned(),
            history.to_vec(),
        )?;
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
        self.active_turns.insert(session_id, Some(turn_id.clone()));
        Ok(turn_id)
    }

    pub fn view_source(&mut self, path: PathBuf) -> Result<ViewResource> {
        let session_id = self.active_session_id()?;
        self.client_mut()?.view_source(session_id, path)
    }

    pub fn events(&mut self, wait_ms: u64) -> Result<EventBatch> {
        let session_id = self.active_session_id()?;
        let after_sequence = self.event_cursors.get(&session_id).copied().unwrap_or(0);
        let batch = self
            .client_mut()?
            .events(session_id.clone(), after_sequence, wait_ms)?;
        if let Some(last) = batch.events.last() {
            self.event_cursors.insert(session_id.clone(), last.sequence);
        }
        self.active_turns
            .insert(session_id, batch.active_turn_id.clone());
        Ok(batch)
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
        let session_id = self.active.as_ref()?.id.as_str();
        self.active_turns.get(session_id).and_then(Option::as_deref)
    }

    pub fn mark_turn_finished(&mut self) {
        if let Some(session_id) = self.active.as_ref().map(|session| session.id.clone()) {
            self.active_turns.insert(session_id, None);
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
        self.active_turns.remove(&current_id);
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

    fn ensure_event_cursor(&mut self, session_id: &str) -> Result<()> {
        if self.event_cursors.contains_key(session_id) {
            return Ok(());
        }
        let (cursor, active_turn) = initial_event_cursor(self.client_mut()?, session_id)?;
        self.event_cursors.insert(session_id.to_owned(), cursor);
        self.active_turns.insert(session_id.to_owned(), active_turn);
        Ok(())
    }
}

fn initial_event_cursor(
    client: &mut SessionClient,
    session_id: &str,
) -> Result<(u64, Option<String>)> {
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
    Ok((cursor, batch.active_turn_id))
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
}

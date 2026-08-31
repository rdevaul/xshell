use crate::config::ActiveModel;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use xshell_core::ChatMessage;
use xshell_session::{
    PersistenceMode, SessionClient, SessionConfig, SessionCreation, SessionDescriptor,
    SessionSnapshot, SessionStatus, Visibility,
};

pub struct SessionRuntime {
    client: Option<SessionClient>,
    active: Option<SessionDescriptor>,
    socket: Option<PathBuf>,
    navigation_history: Vec<String>,
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
        Ok((
            Self {
                client: Some(client),
                active,
                socket: Some(socket),
                navigation_history: Vec::new(),
            },
            Some(snapshot),
        ))
    }

    pub fn disabled() -> Self {
        Self {
            client: None,
            active: None,
            socket: None,
            navigation_history: Vec::new(),
        }
    }

    pub fn active(&self) -> Option<&SessionDescriptor> {
        self.active.as_ref()
    }

    pub fn socket(&self) -> Option<&Path> {
        self.socket.as_deref()
    }

    pub fn list(&mut self) -> Result<Vec<SessionDescriptor>> {
        self.client_mut()?.list()
    }

    pub fn session_names(&mut self) -> Result<Vec<String>> {
        let Some(client) = self.client.as_mut() else {
            return Ok(Vec::new());
        };
        Ok(client
            .list()?
            .into_iter()
            .map(|session| session.name)
            .collect())
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
        let snapshot = self.client_mut()?.switch(selector.to_owned())?;
        if let Some(previous) = previous
            && previous != snapshot.descriptor.id
        {
            self.navigation_history.push(previous);
        }
        self.active = Some(snapshot.descriptor.clone());
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
        Ok(snapshot)
    }

    pub fn detach(&mut self) -> Result<Option<String>> {
        let detached = self.client_mut()?.detach()?;
        self.active = None;
        Ok(detached)
    }

    pub fn close_current_and_fallback(&mut self) -> Result<Option<SessionSnapshot>> {
        let current_id = self
            .active
            .as_ref()
            .map(|session| session.id.clone())
            .context("there is no active session to close")?;
        let catalog = self.client_mut()?.list()?;
        self.client_mut()?.close(None)?;
        self.active = None;
        self.navigation_history
            .retain(|session_id| session_id != &current_id);

        let candidates = fallback_candidates(&self.navigation_history, catalog, &current_id);

        let mut fallback = None;
        for candidate in candidates {
            if let Ok(snapshot) = self.client_mut()?.attach(candidate) {
                self.navigation_history
                    .retain(|session_id| session_id != &snapshot.descriptor.id);
                self.active = Some(snapshot.descriptor.clone());
                fallback = Some(snapshot);
                break;
            }
        }
        Ok(fallback)
    }

    fn client_mut(&mut self) -> Result<&mut SessionClient> {
        self.client
            .as_mut()
            .context("session fabric is disabled; enable [session_fabric] and start xshelld")
    }
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

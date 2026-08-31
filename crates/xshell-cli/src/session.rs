use crate::config::ActiveModel;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use xshell_core::ChatMessage;
use xshell_session::{
    PersistenceMode, SessionClient, SessionConfig, SessionCreation, SessionDescriptor,
    SessionSnapshot, Visibility,
};

pub struct SessionRuntime {
    client: Option<SessionClient>,
    active: Option<SessionDescriptor>,
    socket: Option<PathBuf>,
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
            },
            Some(snapshot),
        ))
    }

    pub fn disabled() -> Self {
        Self {
            client: None,
            active: None,
            socket: None,
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
        let snapshot = self.client_mut()?.switch(selector.to_owned())?;
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
        let snapshot = self.client_mut()?.create(SessionCreation {
            name,
            model: model.to_session_binding(),
            cwd: cwd.to_owned(),
            persistence,
            visibility,
            history,
        })?;
        self.active = Some(snapshot.descriptor.clone());
        Ok(snapshot)
    }

    pub fn detach(&mut self) -> Result<Option<String>> {
        let detached = self.client_mut()?.detach()?;
        self.active = None;
        Ok(detached)
    }

    pub fn close_current(&mut self) -> Result<String> {
        let id = self.client_mut()?.close(None)?;
        self.active = None;
        Ok(id)
    }

    fn client_mut(&mut self) -> Result<&mut SessionClient> {
        self.client
            .as_mut()
            .context("session fabric is disabled; enable [session_fabric] and start xshelld")
    }
}

fn resolve_socket(config: &SessionConfig) -> Result<PathBuf> {
    config
        .resolved_socket()
        .context("HOME and XDG_STATE_HOME are not set; configure session_fabric.socket")
}

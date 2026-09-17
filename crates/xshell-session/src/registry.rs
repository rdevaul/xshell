use crate::{
    AccessMode, AttachmentRole, ModelBinding, PersistenceMode, SessionActivity, SessionCreation,
    SessionDescriptor, SessionSnapshot, SessionStatus,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;
use xshell_execution::validate_working_directory;

#[derive(Debug)]
struct SessionRecord {
    snapshot: SessionSnapshot,
    attachments: HashSet<String>,
}

#[derive(Debug)]
pub struct SessionRegistry {
    host_id: String,
    host_alias: String,
    user: String,
    state_directory: PathBuf,
    sessions: HashMap<String, SessionRecord>,
    in_flight: HashMap<String, DurableInFlight>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DurableState {
    format_version: u32,
    sessions: Vec<SessionSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    in_flight: Vec<DurableInFlight>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DurableInFlight {
    session_id: String,
    operation_id: String,
    route: String,
}

impl SessionRegistry {
    pub fn load(
        state_directory: PathBuf,
        host_id: String,
        host_alias: String,
        user: String,
    ) -> Result<Self> {
        ensure_state_directory(&state_directory)?;
        let state_path = state_directory.join("sessions.json");
        let mut sessions = HashMap::new();
        let mut names = HashSet::new();
        let mut recovered_in_flight = false;
        if state_path.exists() {
            let source = fs::read_to_string(&state_path)
                .with_context(|| format!("cannot read session state {}", state_path.display()))?;
            let state: DurableState = serde_json::from_str(&source)
                .with_context(|| format!("invalid session state {}", state_path.display()))?;
            if state.format_version != 1 {
                bail!("unsupported session state format {}", state.format_version);
            }
            for mut snapshot in state.sessions {
                validate_name(&snapshot.descriptor.name)?;
                snapshot.descriptor.cwd = validate_working_directory(&snapshot.descriptor.cwd)
                    .with_context(|| {
                        format!(
                            "invalid working directory for durable session {:?}",
                            snapshot.descriptor.name
                        )
                    })?;
                if snapshot.descriptor.persistence != PersistenceMode::Durable {
                    bail!("non-durable session found in durable state");
                }
                if !names.insert(snapshot.descriptor.name.clone()) {
                    bail!("duplicate session name in durable state");
                }
                snapshot.descriptor.host_id.clone_from(&host_id);
                snapshot.descriptor.host_alias.clone_from(&host_alias);
                snapshot.descriptor.user.clone_from(&user);
                snapshot.descriptor.status = SessionStatus::Detached;
                snapshot.descriptor.activity = SessionActivity::Idle;
                snapshot.descriptor.attached_clients = 0;
                if sessions
                    .insert(
                        snapshot.descriptor.id.clone(),
                        SessionRecord {
                            snapshot,
                            attachments: HashSet::new(),
                        },
                    )
                    .is_some()
                {
                    bail!("duplicate session ID in durable state");
                }
            }
            for operation in state.in_flight {
                let record = sessions.get_mut(&operation.session_id).with_context(|| {
                    format!(
                        "in-flight operation {:?} references an unknown durable session",
                        operation.operation_id
                    )
                })?;
                let recovery = format!(
                    "[xshell recovery] outcome_unknown: {route} operation {operation_id} was interrupted before a terminal outcome was persisted; inspect external effects before retrying",
                    route = operation.route,
                    operation_id = operation.operation_id,
                );
                if record
                    .snapshot
                    .history
                    .last()
                    .map(|message| message.content.as_str())
                    != Some(recovery.as_str())
                {
                    record
                        .snapshot
                        .history
                        .push(xshell_core::ChatMessage::system(recovery));
                }
                recovered_in_flight = true;
            }
        }
        let registry = Self {
            host_id,
            host_alias,
            user,
            state_directory,
            sessions,
            in_flight: HashMap::new(),
        };
        if recovered_in_flight {
            registry.persist()?;
        }
        Ok(registry)
    }

    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    pub fn host_alias(&self) -> &str {
        &self.host_alias
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn list(&self) -> Vec<SessionDescriptor> {
        let mut sessions = self
            .sessions
            .values()
            .map(descriptor_with_status)
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| left.name.cmp(&right.name));
        sessions
    }

    pub fn create(
        &mut self,
        client_id: &str,
        mut creation: SessionCreation,
    ) -> Result<SessionSnapshot> {
        validate_name(&creation.name)?;
        creation.cwd = validate_working_directory(&creation.cwd)?;
        if self
            .sessions
            .values()
            .any(|record| record.snapshot.descriptor.name == creation.name)
        {
            bail!(
                "a session named {:?} already exists on this host",
                creation.name
            );
        }
        let now = timestamp_ms()?;
        let id = Uuid::new_v4().to_string();
        let descriptor = SessionDescriptor {
            id: id.clone(),
            name: creation.name,
            host_id: self.host_id.clone(),
            host_alias: self.host_alias.clone(),
            user: self.user.clone(),
            model: creation.model,
            cwd: creation.cwd,
            persistence: creation.persistence,
            visibility: creation.visibility,
            access_mode: AccessMode::SingleUser,
            status: SessionStatus::Attached,
            activity: SessionActivity::Idle,
            attached_clients: 1,
            created_at_unix_ms: now,
            last_active_at_unix_ms: now,
        };
        let snapshot = SessionSnapshot {
            descriptor,
            history: creation.history,
        };
        self.sessions.insert(
            id,
            SessionRecord {
                snapshot: snapshot.clone(),
                attachments: HashSet::from([client_id.to_owned()]),
            },
        );
        self.persist()?;
        Ok(snapshot)
    }

    pub fn attach(
        &mut self,
        client_id: &str,
        selector: &str,
        role: AttachmentRole,
    ) -> Result<SessionSnapshot> {
        if role != AttachmentRole::Owner {
            bail!("operator and viewer roles are reserved for future multi-user sessions");
        }
        let id = self.resolve(selector)?;
        let record = self.sessions.get_mut(&id).expect("resolved session exists");
        if !record.attachments.is_empty() && !record.attachments.contains(client_id) {
            bail!(
                "session {:?} already has an interactive controller",
                record.snapshot.descriptor.name
            );
        }
        record.attachments.insert(client_id.to_owned());
        record.snapshot.descriptor.last_active_at_unix_ms = timestamp_ms()?;
        Ok(snapshot_with_status(record))
    }

    pub fn detach(&mut self, client_id: &str, session_id: &str) -> Result<Option<String>> {
        let Some(record) = self.sessions.get_mut(session_id) else {
            return Ok(None);
        };
        record.attachments.remove(client_id);
        let remove = record.attachments.is_empty()
            && record.snapshot.descriptor.persistence == PersistenceMode::Ephemeral;
        if remove {
            self.sessions.remove(session_id);
        }
        self.persist()?;
        Ok(Some(session_id.to_owned()))
    }

    pub fn update(
        &mut self,
        client_id: &str,
        session_id: &str,
        model: ModelBinding,
        cwd: PathBuf,
        history: Vec<xshell_core::ChatMessage>,
    ) -> Result<SessionDescriptor> {
        let cwd = validate_working_directory(&cwd)?;
        let record = self
            .sessions
            .get_mut(session_id)
            .with_context(|| format!("unknown session {session_id:?}"))?;
        if !record.attachments.contains(client_id) {
            bail!(
                "client is not attached to session {:?}",
                record.snapshot.descriptor.name
            );
        }
        record.snapshot.descriptor.model = model;
        record.snapshot.descriptor.cwd = cwd;
        record.snapshot.descriptor.last_active_at_unix_ms = timestamp_ms()?;
        record.snapshot.history = history;
        let descriptor = descriptor_with_status(record);
        self.persist()?;
        Ok(descriptor)
    }

    pub fn begin_execution(
        &mut self,
        session_id: &str,
        operation_id: &str,
        route: &str,
    ) -> Result<()> {
        let record = self
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id:?}"))?;
        if record.snapshot.descriptor.persistence != PersistenceMode::Durable {
            return Ok(());
        }
        if self.in_flight.contains_key(session_id) {
            bail!("durable session already has an in-flight operation");
        }
        self.in_flight.insert(
            session_id.to_owned(),
            DurableInFlight {
                session_id: session_id.to_owned(),
                operation_id: operation_id.to_owned(),
                route: route.to_owned(),
            },
        );
        if let Err(error) = self.persist() {
            self.in_flight.remove(session_id);
            return Err(error);
        }
        Ok(())
    }

    pub fn finish_execution(&mut self, session_id: &str, operation_id: &str) -> Result<()> {
        if self
            .in_flight
            .get(session_id)
            .is_some_and(|operation| operation.operation_id == operation_id)
        {
            self.in_flight.remove(session_id);
            self.persist()?;
        }
        Ok(())
    }

    pub fn update_execution_state(
        &mut self,
        session_id: &str,
        cwd: PathBuf,
        history: Vec<xshell_core::ChatMessage>,
    ) -> Result<SessionDescriptor> {
        let cwd = validate_working_directory(&cwd)?;
        let record = self
            .sessions
            .get_mut(session_id)
            .with_context(|| format!("unknown session {session_id:?}"))?;
        record.snapshot.descriptor.cwd = cwd;
        record.snapshot.descriptor.last_active_at_unix_ms = timestamp_ms()?;
        record.snapshot.history = history;
        let descriptor = descriptor_with_status(record);
        self.in_flight.remove(session_id);
        self.persist()?;
        Ok(descriptor)
    }

    pub fn close(&mut self, client_id: &str, selector: &str) -> Result<String> {
        let id = self.resolve(selector)?;
        let record = self.sessions.get(&id).expect("resolved session exists");
        if record
            .attachments
            .iter()
            .any(|attached| attached != client_id)
        {
            bail!(
                "session {:?} is controlled by another client",
                record.snapshot.descriptor.name
            );
        }
        self.sessions.remove(&id);
        self.persist()?;
        Ok(id)
    }

    pub fn snapshot(&self, selector: &str) -> Result<SessionSnapshot> {
        let id = self.resolve(selector)?;
        Ok(snapshot_with_status(
            self.sessions.get(&id).expect("resolved session exists"),
        ))
    }

    fn resolve(&self, selector: &str) -> Result<String> {
        if self.sessions.contains_key(selector) {
            return Ok(selector.to_owned());
        }
        let matches = self
            .sessions
            .iter()
            .filter(|(_, record)| record.snapshot.descriptor.name == selector)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [id] => Ok(id.clone()),
            [] => bail!("unknown session {selector:?}"),
            _ => bail!("ambiguous session selector {selector:?}"),
        }
    }

    fn persist(&self) -> Result<()> {
        let mut sessions = self
            .sessions
            .values()
            .filter(|record| record.snapshot.descriptor.persistence == PersistenceMode::Durable)
            .map(|record| {
                let mut snapshot = record.snapshot.clone();
                snapshot.descriptor.status = SessionStatus::Detached;
                snapshot.descriptor.attached_clients = 0;
                snapshot
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| left.descriptor.name.cmp(&right.descriptor.name));
        let encoded = serde_json::to_vec_pretty(&DurableState {
            format_version: 1,
            sessions,
            in_flight: {
                let mut operations = self.in_flight.values().cloned().collect::<Vec<_>>();
                operations.sort_by(|left, right| left.session_id.cmp(&right.session_id));
                operations
            },
        })?;
        let temporary = self.state_directory.join("sessions.json.new");
        let final_path = self.state_directory.join("sessions.json");
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true).mode(0o600);
        let mut file = options.open(&temporary)?;
        use std::io::Write;
        file.write_all(&encoded)?;
        file.sync_all()?;
        fs::rename(&temporary, &final_path)?;
        Ok(())
    }
}

fn descriptor_with_status(record: &SessionRecord) -> SessionDescriptor {
    let mut descriptor = record.snapshot.descriptor.clone();
    descriptor.attached_clients = record.attachments.len() as u32;
    descriptor.status = if record.attachments.is_empty() {
        SessionStatus::Detached
    } else {
        SessionStatus::Attached
    };
    descriptor
}

fn snapshot_with_status(record: &SessionRecord) -> SessionSnapshot {
    let mut snapshot = record.snapshot.clone();
    snapshot.descriptor = descriptor_with_status(record);
    snapshot
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("session names must contain 1 to 64 characters");
    }
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("session names may contain only ASCII letters, digits, '-' and '_'");
    }
    Ok(())
}

fn ensure_state_directory(path: &Path) -> Result<()> {
    xshell_platform::ensure_secure_directory(path, "session state")
}

fn timestamp_ms() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()
        .context("timestamp exceeds session format")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Visibility;
    use tempfile::TempDir;
    use xshell_core::ChatMessage;

    fn model() -> ModelBinding {
        ModelBinding {
            profile_name: Some("local".into()),
            provider: "ollama".into(),
            model: "qwen".into(),
            base_url: "http://localhost:11434".into(),
            api_key_env: None,
            max_history_bytes: None,
        }
    }

    fn creation(
        name: &str,
        cwd: &Path,
        persistence: PersistenceMode,
        visibility: Visibility,
        history: Vec<ChatMessage>,
    ) -> SessionCreation {
        SessionCreation {
            name: name.into(),
            model: model(),
            cwd: cwd.into(),
            persistence,
            visibility,
            history,
        }
    }

    #[test]
    fn daemon_sessions_survive_detach_but_ephemeral_sessions_do_not() {
        let temp = TempDir::new().unwrap();
        let mut registry = SessionRegistry::load(
            temp.path().into(),
            "host".into(),
            "local".into(),
            "rich".into(),
        )
        .unwrap();
        let daemon = registry
            .create(
                "client",
                creation(
                    "bees",
                    temp.path(),
                    PersistenceMode::Daemon,
                    Visibility::Fabric,
                    Vec::new(),
                ),
            )
            .unwrap();
        registry.detach("client", &daemon.descriptor.id).unwrap();
        assert_eq!(registry.list().len(), 1);

        let ephemeral = registry
            .create(
                "client",
                creation(
                    "scratch",
                    temp.path(),
                    PersistenceMode::Ephemeral,
                    Visibility::HostOnly,
                    Vec::new(),
                ),
            )
            .unwrap();
        registry.detach("client", &ephemeral.descriptor.id).unwrap();
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn durable_sessions_restore_after_registry_restart() {
        let temp = TempDir::new().unwrap();
        let mut registry = SessionRegistry::load(
            temp.path().into(),
            "host".into(),
            "local".into(),
            "rich".into(),
        )
        .unwrap();
        let durable = registry
            .create(
                "client",
                creation(
                    "ornithopter",
                    temp.path(),
                    PersistenceMode::Durable,
                    Visibility::Fabric,
                    vec![ChatMessage::user("retain me")],
                ),
            )
            .unwrap();
        registry.detach("client", &durable.descriptor.id).unwrap();
        drop(registry);

        let restored = SessionRegistry::load(
            temp.path().into(),
            "host".into(),
            "local".into(),
            "rich".into(),
        )
        .unwrap();
        let snapshot = restored.snapshot("ornithopter").unwrap();
        assert_eq!(snapshot.history, vec![ChatMessage::user("retain me")]);
        assert_eq!(snapshot.descriptor.status, SessionStatus::Detached);
    }

    #[test]
    fn f3_interrupted_durable_operation_recovers_as_outcome_unknown() {
        let temp = TempDir::new().unwrap();
        let mut registry = SessionRegistry::load(
            temp.path().into(),
            "host".into(),
            "local".into(),
            "rich".into(),
        )
        .unwrap();
        let durable = registry
            .create(
                "client",
                creation(
                    "interrupted",
                    temp.path(),
                    PersistenceMode::Durable,
                    Visibility::Fabric,
                    vec![ChatMessage::user("before")],
                ),
            )
            .unwrap();
        registry
            .begin_execution(&durable.descriptor.id, "turn-123", "shell")
            .unwrap();
        drop(registry);

        let restored = SessionRegistry::load(
            temp.path().into(),
            "host".into(),
            "local".into(),
            "rich".into(),
        )
        .unwrap();
        let snapshot = restored.snapshot("interrupted").unwrap();
        let recovery = snapshot.history.last().unwrap();
        assert!(recovery.content.contains("outcome_unknown"));
        assert!(recovery.content.contains("turn-123"));
        assert!(recovery.content.contains("inspect external effects"));

        let persisted = std::fs::read_to_string(temp.path().join("sessions.json")).unwrap();
        let persisted: serde_json::Value = serde_json::from_str(&persisted).unwrap();
        assert!(persisted.get("in_flight").is_none());
        let persisted = serde_json::to_string(&persisted).unwrap();
        assert_eq!(persisted.matches("outcome_unknown").count(), 1);
    }
}

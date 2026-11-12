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
use xshell_execution::{ImageStore, ImageStoreLimits, validate_working_directory};

#[derive(Debug, Clone)]
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
    image_limits: ImageStoreLimits,
    /// Most sessions kept at once; creating another is refused.
    max_sessions: usize,
    /// Tests: make the next `persist` fail at this step.
    #[cfg(test)]
    fail_next_persist: std::cell::Cell<Option<PersistStep>>,
}

/// The steps of `persist` before which a test can inject a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistStep {
    Write,
    Sync,
    Rename,
}

/// Directory under the state directory that holds each session's image
/// store.
const IMAGES_DIRECTORY: &str = "images";

/// Format of `sessions.json` written by this daemon.
///
/// - 2: messages in session history may carry `images` (image references).
///   Version 1 files load unchanged; an older daemon refuses a version 2 file
///   rather than dropping the images it cannot represent.
/// - 3: session descriptors may carry `tools` (a tool selection). An older
///   daemon refuses a version 3 file rather than dropping a selection, which
///   would give that session's agent every tool.
const SESSION_STATE_FORMAT_VERSION: u32 = 3;
/// Oldest `sessions.json` format this daemon still loads.
const MIN_SESSION_STATE_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct DurableState {
    format_version: u32,
    sessions: Vec<SessionSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    in_flight: Vec<DurableInFlight>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
            if !(MIN_SESSION_STATE_FORMAT_VERSION..=SESSION_STATE_FORMAT_VERSION)
                .contains(&state.format_version)
            {
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
            image_limits: ImageStoreLimits::default(),
            max_sessions: crate::DEFAULT_MAX_SESSIONS,
            #[cfg(test)]
            fail_next_persist: std::cell::Cell::new(None),
        };
        if recovered_in_flight {
            registry.persist()?;
        }
        registry.collect_images();
        Ok(registry)
    }

    /// Set the most sessions this registry keeps. Sessions restored beyond
    /// it stay; only new ones are refused.
    pub fn set_max_sessions(&mut self, max_sessions: usize) {
        self.max_sessions = max_sessions;
    }

    pub fn state_directory(&self) -> &Path {
        &self.state_directory
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
        if self.sessions.len() >= self.max_sessions {
            return Err(crate::CapacityExhausted {
                message: format!(
                    "this host already has {} sessions, its limit (session_fabric.max_sessions); \
close one to create another",
                    self.sessions.len()
                ),
            }
            .into());
        }
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
            tools: creation.tools,
        };
        // A new session's store is empty, so history from the client may not
        // reference any image yet.
        if let Some(image) = creation.history.iter().flat_map(|m| &m.images).next() {
            bail!("image {} is not in this session's store", image.sha256());
        }
        let snapshot = SessionSnapshot {
            descriptor,
            history: creation.history,
        };
        let record = SessionRecord {
            snapshot: snapshot.clone(),
            attachments: HashSet::from([client_id.to_owned()]),
        };
        self.commit(&id.clone(), |registry| {
            registry.sessions.insert(id, record);
            Ok(())
        })?;
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
        if !self.sessions.contains_key(session_id) {
            return Ok(None);
        }
        let removed = self.commit(session_id, |registry| {
            let record = registry
                .sessions
                .get_mut(session_id)
                .expect("checked above");
            record.attachments.remove(client_id);
            let remove = record.attachments.is_empty()
                && record.snapshot.descriptor.persistence == PersistenceMode::Ephemeral;
            if remove {
                registry.sessions.remove(session_id);
            }
            Ok(remove)
        })?;
        // Effects outside the registry follow only a committed change.
        if removed {
            self.remove_images(session_id);
        }
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
            .get(session_id)
            .with_context(|| format!("unknown session {session_id:?}"))?;
        if !record.attachments.contains(client_id) {
            bail!(
                "client is not attached to session {:?}",
                record.snapshot.descriptor.name
            );
        }
        let mut images = history
            .iter()
            .flat_map(|message| &message.images)
            .peekable();
        // A text-only history needs no store, so a store problem cannot
        // break an ordinary update.
        if images.peek().is_some() {
            ImageStore::open(
                &self.state_directory.join(IMAGES_DIRECTORY),
                session_id,
                self.image_limits,
            )?
            .require_all(images)
            .context("the history references an image this session does not have")?;
        }
        let now = timestamp_ms()?;
        self.commit(session_id, |registry| {
            let record = registry
                .sessions
                .get_mut(session_id)
                .expect("checked above");
            record.snapshot.descriptor.model = model;
            record.snapshot.descriptor.cwd = cwd;
            record.snapshot.descriptor.last_active_at_unix_ms = now;
            record.snapshot.history = history;
            Ok(descriptor_with_status(record))
        })
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
        self.commit(session_id, |registry| {
            registry.in_flight.insert(
                session_id.to_owned(),
                DurableInFlight {
                    session_id: session_id.to_owned(),
                    operation_id: operation_id.to_owned(),
                    route: route.to_owned(),
                },
            );
            Ok(())
        })
    }

    pub fn finish_execution(&mut self, session_id: &str, operation_id: &str) -> Result<()> {
        if self
            .in_flight
            .get(session_id)
            .is_some_and(|operation| operation.operation_id == operation_id)
        {
            self.commit(session_id, |registry| {
                registry.in_flight.remove(session_id);
                Ok(())
            })?;
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
        if !self.sessions.contains_key(session_id) {
            bail!("unknown session {session_id:?}");
        }
        let now = timestamp_ms()?;
        self.commit(session_id, |registry| {
            let record = registry
                .sessions
                .get_mut(session_id)
                .expect("checked above");
            record.snapshot.descriptor.cwd = cwd;
            record.snapshot.descriptor.last_active_at_unix_ms = now;
            record.snapshot.history = history;
            let descriptor = descriptor_with_status(record);
            registry.in_flight.remove(session_id);
            Ok(descriptor)
        })
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
        self.commit(&id, |registry| {
            registry.sessions.remove(&id);
            Ok(())
        })?;
        self.remove_images(&id);
        Ok(id)
    }

    /// The image store of a live session.
    pub fn image_store(&self, session_id: &str) -> Result<ImageStore> {
        if !self.sessions.contains_key(session_id) {
            bail!("unknown session {session_id:?}");
        }
        ImageStore::open(&self.images_root(), session_id, self.image_limits)
    }

    fn images_root(&self) -> PathBuf {
        self.state_directory.join(IMAGES_DIRECTORY)
    }

    /// Remove a closed session's images. A failure is not the close's
    /// failure: the session is already gone, and the next daemon start
    /// removes stores that belong to no session.
    fn remove_images(&self, session_id: &str) {
        let root = self.images_root();
        if let Err(error) =
            ImageStore::open(&root, session_id, self.image_limits).and_then(ImageStore::remove)
        {
            eprintln!(
                "xshelld: cannot remove the images of closed session {session_id}: {error:#}"
            );
        }
    }

    /// At startup, remove stores that belong to no session (ephemeral
    /// sessions do not survive a restart) and images no durable session's
    /// history references any more.
    ///
    /// Collection runs only here and when a session ends, never on a history
    /// update: an image stored for a message the user has not sent yet would
    /// look unreferenced. The store limits bound growth in between.
    ///
    /// Collection is best-effort: a store it cannot clean is reported and
    /// left for the next start, and never stops the daemon from serving
    /// its sessions.
    fn collect_images(&self) {
        let root = self.images_root();
        match xshell_execution::remove_orphan_stores(
            &root,
            self.sessions.keys().map(String::as_str),
        ) {
            Ok(report) => {
                for error in report.errors {
                    eprintln!("xshelld warning: cannot remove an orphaned image store: {error:#}");
                }
            }
            Err(error) => {
                eprintln!("xshelld warning: cannot collect unused images: {error:#}");
                return;
            }
        }
        for (id, record) in &self.sessions {
            if !root.join(id).exists() {
                continue;
            }
            let result = ImageStore::open(&root, id, self.image_limits).and_then(|store| {
                store.retain(record.snapshot.history.iter().flat_map(|m| &m.images))
            });
            if let Err(error) = result {
                eprintln!(
                    "xshelld warning: cannot collect unused images of session {id}: {error:#}"
                );
            }
        }
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

    /// Apply `mutate`, which changes at most the session `session_id` and
    /// its in-flight operation, and make the change durable. If `mutate` or
    /// the write fails, the registry is put back as it was, so the caller's
    /// error, the live registry, and the state a restart would load agree
    /// (F7). Only the registry's own metadata is rolled back: effects an
    /// operation already had outside it (a command that ran, images stored)
    /// are not undone, which is why callers apply such effects only after a
    /// successful commit, or record them as in-flight first.
    ///
    /// Sessions that are not durable are never written, so a change that
    /// touches no durable state skips the write.
    fn commit<T>(
        &mut self,
        session_id: &str,
        mutate: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let record_before = self.sessions.get(session_id).cloned();
        let in_flight_before = self.in_flight.get(session_id).cloned();
        let value = match mutate(self) {
            Ok(value) => value,
            Err(error) => {
                self.restore(session_id, record_before, in_flight_before);
                return Err(error);
            }
        };
        let is_durable = |record: Option<&SessionRecord>| {
            record.is_some_and(|record| {
                record.snapshot.descriptor.persistence == PersistenceMode::Durable
            })
        };
        let touches_durable_state = is_durable(record_before.as_ref())
            || is_durable(self.sessions.get(session_id))
            || in_flight_before.as_ref() != self.in_flight.get(session_id);
        if touches_durable_state && let Err(error) = self.persist() {
            self.restore(session_id, record_before, in_flight_before);
            return Err(error);
        }
        Ok(value)
    }

    fn restore(
        &mut self,
        session_id: &str,
        record: Option<SessionRecord>,
        in_flight: Option<DurableInFlight>,
    ) {
        match record {
            Some(record) => self.sessions.insert(session_id.to_owned(), record),
            None => self.sessions.remove(session_id),
        };
        match in_flight {
            Some(operation) => self.in_flight.insert(session_id.to_owned(), operation),
            None => self.in_flight.remove(session_id),
        };
    }

    /// Write the durable state: a new file, synced, renamed over the old
    /// one, then the directory synced. Until the rename the old state stands
    /// and an error is returned. Once the rename has happened the new state
    /// is what a restart loads, so a failure to sync the directory after it
    /// is reported but does not fail the operation: failing it would roll
    /// the live registry back to a state the disk no longer holds.
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
            format_version: SESSION_STATE_FORMAT_VERSION,
            sessions,
            in_flight: {
                let mut operations = self.in_flight.values().cloned().collect::<Vec<_>>();
                operations.sort_by(|left, right| left.session_id.cmp(&right.session_id));
                operations
            },
        })?;
        let temporary = self.state_directory.join("sessions.json.new");
        let final_path = self.state_directory.join("sessions.json");
        let written = (|| -> Result<()> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create(true).truncate(true).mode(0o600);
            let mut file = options.open(&temporary)?;
            use std::io::Write;
            self.inject_failure(PersistStep::Write)?;
            file.write_all(&encoded)?;
            self.inject_failure(PersistStep::Sync)?;
            file.sync_all()?;
            self.inject_failure(PersistStep::Rename)?;
            fs::rename(&temporary, &final_path)?;
            Ok(())
        })();
        if let Err(error) = written {
            let _ = fs::remove_file(&temporary);
            return Err(error.context("cannot persist session state"));
        }
        if let Err(error) = sync_directory(&self.state_directory) {
            eprintln!(
                "xshelld warning: session state was written but {} could not be synced: {error}",
                self.state_directory.display()
            );
        }
        Ok(())
    }

    #[cfg(test)]
    fn inject_failure(&self, step: PersistStep) -> Result<()> {
        if self.fail_next_persist.get() == Some(step) {
            self.fail_next_persist.set(None);
            bail!("injected {step:?} failure");
        }
        Ok(())
    }

    #[cfg(not(test))]
    #[inline]
    fn inject_failure(&self, _step: PersistStep) -> Result<()> {
        Ok(())
    }
}

/// Sync a directory, so a rename inside it survives a crash.
pub(crate) fn sync_directory(path: &Path) -> std::io::Result<()> {
    fs::File::open(path)?.sync_all()
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
    use xshell_core::{ChatMessage, ImageMediaType, ImageRef};

    fn model() -> ModelBinding {
        ModelBinding {
            profile_name: Some("local".into()),
            provider: "ollama".into(),
            model: "qwen".into(),
            base_url: "http://localhost:11434".into(),
            api_key_env: None,
            max_history_bytes: None,
            context_tokens: None,
            reserve_output_tokens: None,
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
            tools: Default::default(),
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

    fn load(path: &Path) -> Result<SessionRegistry> {
        SessionRegistry::load(path.into(), "host".into(), "local".into(), "rich".into())
    }

    const STEPS: [PersistStep; 3] = [PersistStep::Write, PersistStep::Sync, PersistStep::Rename];

    fn history_of(registry: &SessionRegistry, name: &str) -> Vec<ChatMessage> {
        registry.snapshot(name).unwrap().history
    }

    #[test]
    fn f7_a_failed_create_leaves_no_session_in_memory_or_on_disk() {
        for step in STEPS {
            let temp = TempDir::new().unwrap();
            let mut registry = load(temp.path()).unwrap();
            registry.fail_next_persist.set(Some(step));
            let error = registry
                .create(
                    "client",
                    creation(
                        "ornithopter",
                        temp.path(),
                        PersistenceMode::Durable,
                        Visibility::Fabric,
                        Vec::new(),
                    ),
                )
                .unwrap_err();
            assert!(
                format!("{error:#}").contains("injected"),
                "{step:?}: {error:#}"
            );
            assert!(
                registry.list().is_empty(),
                "{step:?}: live registry kept it"
            );
            assert!(
                load(temp.path()).unwrap().list().is_empty(),
                "{step:?}: disk kept it"
            );
            assert!(
                !temp.path().join("sessions.json.new").exists(),
                "{step:?}: the partial file was left behind"
            );
            // The name is free again.
            registry
                .create(
                    "client",
                    creation(
                        "ornithopter",
                        temp.path(),
                        PersistenceMode::Durable,
                        Visibility::Fabric,
                        Vec::new(),
                    ),
                )
                .unwrap();
        }
    }

    #[test]
    fn f7_a_failed_update_keeps_the_previous_history_everywhere() {
        for step in STEPS {
            let temp = TempDir::new().unwrap();
            create_durable(temp.path(), vec![ChatMessage::user("before")]);
            let mut registry = load(temp.path()).unwrap();
            let id = registry.snapshot("ornithopter").unwrap().descriptor.id;
            registry
                .attach("client", &id, AttachmentRole::Owner)
                .unwrap();
            registry.fail_next_persist.set(Some(step));
            assert!(
                registry
                    .update(
                        "client",
                        &id,
                        model(),
                        temp.path().into(),
                        vec![ChatMessage::user("after")],
                    )
                    .is_err()
            );
            assert_eq!(
                history_of(&registry, "ornithopter"),
                vec![ChatMessage::user("before")],
                "{step:?}"
            );
            assert_eq!(
                history_of(&load(temp.path()).unwrap(), "ornithopter"),
                vec![ChatMessage::user("before")],
                "{step:?}"
            );
            // The same update succeeds once the disk does.
            registry
                .update(
                    "client",
                    &id,
                    model(),
                    temp.path().into(),
                    vec![ChatMessage::user("after")],
                )
                .unwrap();
            assert_eq!(
                history_of(&load(temp.path()).unwrap(), "ornithopter"),
                vec![ChatMessage::user("after")]
            );
        }
    }

    #[test]
    fn f7_a_failed_close_keeps_the_session_everywhere() {
        for step in STEPS {
            let temp = TempDir::new().unwrap();
            create_durable(temp.path(), vec![ChatMessage::user("keep")]);
            let mut registry = load(temp.path()).unwrap();
            registry.fail_next_persist.set(Some(step));
            assert!(registry.close("client", "ornithopter").is_err(), "{step:?}");
            assert_eq!(registry.list().len(), 1, "{step:?}");
            assert_eq!(load(temp.path()).unwrap().list().len(), 1, "{step:?}");
        }
    }

    #[test]
    fn f7_a_failed_execution_state_update_keeps_the_in_flight_marker() {
        let temp = TempDir::new().unwrap();
        create_durable(temp.path(), vec![ChatMessage::user("before")]);
        let mut registry = load(temp.path()).unwrap();
        let id = registry.snapshot("ornithopter").unwrap().descriptor.id;
        registry.begin_execution(&id, "turn-1", "agent").unwrap();
        registry.fail_next_persist.set(Some(PersistStep::Rename));
        assert!(
            registry
                .update_execution_state(&id, temp.path().into(), vec![ChatMessage::user("x")])
                .is_err()
        );
        assert!(registry.in_flight.contains_key(&id));
        assert_eq!(
            history_of(&registry, "ornithopter"),
            vec![ChatMessage::user("before")]
        );
        // A restart sees the interrupted operation, as the live registry does.
        let restored = load(temp.path()).unwrap();
        assert!(
            history_of(&restored, "ornithopter")
                .last()
                .unwrap()
                .content
                .contains("outcome_unknown")
        );
    }

    #[test]
    fn f7_changes_to_sessions_that_are_not_durable_write_nothing() {
        let temp = TempDir::new().unwrap();
        let mut registry = load(temp.path()).unwrap();
        // A failure would be injected into a write, but none happens.
        registry.fail_next_persist.set(Some(PersistStep::Write));
        let session = registry
            .create(
                "client",
                creation(
                    "scratch",
                    temp.path(),
                    PersistenceMode::Daemon,
                    Visibility::Fabric,
                    Vec::new(),
                ),
            )
            .unwrap();
        registry.detach("client", &session.descriptor.id).unwrap();
        assert_eq!(registry.fail_next_persist.get(), Some(PersistStep::Write));
    }

    #[test]
    fn a_durable_session_keeps_its_tool_selection_across_restarts() {
        let temp = TempDir::new().unwrap();
        let load = || {
            SessionRegistry::load(
                temp.path().into(),
                "host".into(),
                "local".into(),
                "rich".into(),
            )
            .unwrap()
        };
        let only = crate::SessionTools::Only(vec!["read_file".into(), "memory_get".into()]);
        let mut registry = load();
        for (name, tools) in [("chat", only.clone()), ("full", crate::SessionTools::All)] {
            let mut creation = creation(
                name,
                temp.path(),
                PersistenceMode::Durable,
                Visibility::Fabric,
                Vec::new(),
            );
            creation.tools = tools;
            let created = registry.create("client", creation).unwrap();
            registry.detach("client", &created.descriptor.id).unwrap();
        }
        drop(registry);

        let restored = load();
        assert_eq!(restored.snapshot("chat").unwrap().descriptor.tools, only);
        assert_eq!(
            restored.snapshot("full").unwrap().descriptor.tools,
            crate::SessionTools::All
        );
        // An unrestricted session is saved without the field, so state written
        // before selections existed reads back as every tool.
        let saved = std::fs::read_to_string(temp.path().join("sessions.json")).unwrap();
        assert_eq!(saved.matches("\"tools\"").count(), 1, "{saved}");
    }

    fn create_durable(path: &Path, history: Vec<ChatMessage>) {
        let mut registry = load(path).unwrap();
        let durable = registry
            .create(
                "client",
                creation(
                    "ornithopter",
                    path,
                    PersistenceMode::Durable,
                    Visibility::Fabric,
                    history,
                ),
            )
            .unwrap();
        registry.detach("client", &durable.descriptor.id).unwrap();
    }

    fn set_format_version(path: &Path, version: u32) {
        let state_path = path.join("sessions.json");
        let mut state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        state["format_version"] = version.into();
        std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    }

    fn put_image(registry: &SessionRegistry, session_id: &str, bytes: &[u8]) -> ImageRef {
        registry
            .image_store(session_id)
            .unwrap()
            .put(
                bytes,
                ImageMediaType::Png,
                1568,
                1176,
                "renders/bracket.png",
            )
            .unwrap()
    }

    fn create_session(
        registry: &mut SessionRegistry,
        path: &Path,
        name: &str,
        persistence: PersistenceMode,
    ) -> String {
        registry
            .create(
                "client",
                creation(name, path, persistence, Visibility::Fabric, Vec::new()),
            )
            .unwrap()
            .descriptor
            .id
    }

    #[test]
    fn stored_images_survive_restart_and_unreferenced_ones_are_collected() {
        let temp = TempDir::new().unwrap();
        let mut registry = load(temp.path()).unwrap();
        let id = create_session(
            &mut registry,
            temp.path(),
            "ornithopter",
            PersistenceMode::Durable,
        );
        let kept = put_image(&registry, &id, b"referenced render");
        let unsent = put_image(&registry, &id, b"never sent");
        let history = vec![ChatMessage::user("look at this").with_images([kept.clone()])];
        registry
            .update("client", &id, model(), temp.path().into(), history.clone())
            .unwrap();
        // Not collected while the daemon runs: it may be about to be sent.
        assert!(registry.image_store(&id).unwrap().contains(&unsent));
        registry.detach("client", &id).unwrap();
        drop(registry);

        let persisted = std::fs::read_to_string(temp.path().join("sessions.json")).unwrap();
        let persisted: serde_json::Value = serde_json::from_str(&persisted).unwrap();
        assert_eq!(persisted["format_version"], SESSION_STATE_FORMAT_VERSION);

        let restored = load(temp.path()).unwrap();
        assert_eq!(restored.snapshot("ornithopter").unwrap().history, history);
        let store = restored.image_store(&id).unwrap();
        assert_eq!(store.get(&kept).unwrap(), b"referenced render");
        assert!(!store.contains(&unsent));
    }

    #[test]
    fn history_may_reference_only_this_sessions_images() {
        let temp = TempDir::new().unwrap();
        let mut registry = load(temp.path()).unwrap();
        let first = create_session(
            &mut registry,
            temp.path(),
            "first",
            PersistenceMode::Durable,
        );
        let second = create_session(
            &mut registry,
            temp.path(),
            "second",
            PersistenceMode::Durable,
        );
        let image = put_image(&registry, &first, b"first's render");

        let history = vec![ChatMessage::user("steal").with_images([image.clone()])];
        let error = registry
            .update(
                "client",
                &second,
                model(),
                temp.path().into(),
                history.clone(),
            )
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("not in this session's store"),
            "{error:#}"
        );
        assert!(registry.snapshot("second").unwrap().history.is_empty());

        let error = registry
            .create(
                "client",
                creation(
                    "third",
                    temp.path(),
                    PersistenceMode::Durable,
                    Visibility::Fabric,
                    history,
                ),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("not in this session's store"),
            "{error:#}"
        );
        assert!(registry.snapshot("third").is_err());
    }

    #[test]
    fn ending_a_session_removes_its_images() {
        let temp = TempDir::new().unwrap();
        let mut registry = load(temp.path()).unwrap();
        let durable = create_session(
            &mut registry,
            temp.path(),
            "durable",
            PersistenceMode::Durable,
        );
        let ephemeral = create_session(
            &mut registry,
            temp.path(),
            "ephemeral",
            PersistenceMode::Ephemeral,
        );
        put_image(&registry, &durable, b"durable render");
        put_image(&registry, &ephemeral, b"ephemeral render");
        let images = temp.path().join(IMAGES_DIRECTORY);

        registry.detach("client", &ephemeral).unwrap();
        assert!(!images.join(&ephemeral).exists());
        assert!(images.join(&durable).exists());

        registry.close("client", "durable").unwrap();
        assert!(!images.join(&durable).exists());
    }

    #[test]
    fn stores_of_sessions_that_no_longer_exist_are_removed_at_startup() {
        let temp = TempDir::new().unwrap();
        let mut registry = load(temp.path()).unwrap();
        let durable = create_session(
            &mut registry,
            temp.path(),
            "durable",
            PersistenceMode::Durable,
        );
        let ephemeral = create_session(
            &mut registry,
            temp.path(),
            "ephemeral",
            PersistenceMode::Ephemeral,
        );
        let unreferenced = put_image(&registry, &durable, b"unreferenced");
        put_image(&registry, &ephemeral, b"lost when the daemon stopped");
        // The daemon stops without the ephemeral session ending cleanly.
        drop(registry);

        let images = temp.path().join(IMAGES_DIRECTORY);
        assert!(images.join(&ephemeral).exists());
        let restored = load(temp.path()).unwrap();
        assert!(!images.join(&ephemeral).exists());
        // The durable session's store stays; its unreferenced image is
        // collected like any other.
        assert!(images.join(&durable).is_dir());
        assert!(
            !restored
                .image_store(&durable)
                .unwrap()
                .contains(&unreferenced)
        );
    }

    #[test]
    fn an_image_store_that_cannot_be_cleaned_does_not_stop_startup() {
        let temp = TempDir::new().unwrap();
        let mut registry = load(temp.path()).unwrap();
        let durable = create_session(
            &mut registry,
            temp.path(),
            "durable",
            PersistenceMode::Durable,
        );
        put_image(&registry, &durable, b"render");
        drop(registry);
        let images = temp.path().join(IMAGES_DIRECTORY);
        // An orphaned store with something unexpected in it, and a stray
        // directory in the live session's store.
        let orphan = images.join("22222222-2222-4222-8222-222222222222");
        fs::create_dir_all(orphan.join("unexpected")).unwrap();
        fs::create_dir(images.join(&durable).join("unexpected")).unwrap();

        let restored = load(temp.path()).unwrap();
        assert!(restored.snapshot("durable").is_ok());
        assert!(orphan.exists(), "left for the next start");
    }

    #[test]
    fn version_one_session_state_still_loads() {
        let temp = TempDir::new().unwrap();
        create_durable(temp.path(), vec![ChatMessage::user("retain me")]);
        set_format_version(temp.path(), 1);

        let restored = load(temp.path()).unwrap();
        let snapshot = restored.snapshot("ornithopter").unwrap();
        assert_eq!(snapshot.history, vec![ChatMessage::user("retain me")]);
        assert!(snapshot.history[0].images.is_empty());
    }

    #[test]
    fn newer_session_state_formats_are_refused() {
        let temp = TempDir::new().unwrap();
        create_durable(temp.path(), Vec::new());
        set_format_version(temp.path(), SESSION_STATE_FORMAT_VERSION + 1);

        let error = load(temp.path()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported session state format"),
            "{error:#}"
        );
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

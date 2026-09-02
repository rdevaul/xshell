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
use xshell_audit::{AuditClient, AuditConfig, AuditEvent};

/// Per-daemon audit policy plus one audit-service session per xshell session.
#[derive(Clone, Default)]
pub struct DaemonAudit {
    inner: Option<Arc<AuditInner>>,
}

struct AuditInner {
    config: AuditConfig,
    sessions: Mutex<HashMap<String, SessionAudit>>,
}

struct SessionAudit {
    client: Option<AuditClient>,
    audit_session_id: String,
}

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
            let mut probe = AuditClient::connect(socket, env!("CARGO_PKG_VERSION"))
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
        let mut sessions = inner
            .sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !sessions.contains_key(session_id) {
            let opened = inner
                .config
                .socket
                .as_deref()
                .context("audit.socket is required when auditing is enabled")
                .and_then(|socket| AuditClient::connect(socket, env!("CARGO_PKG_VERSION")));
            let entry = match opened {
                Ok(mut client) => {
                    let audit_session_id = client.session_id().to_owned();
                    let started = client.append(AuditEvent::LogicalSessionAttached {
                        action: "xshelld_execution".into(),
                        session_id: session_id.to_owned(),
                        name: descriptor.name,
                        host_id: descriptor.host_id,
                        host_alias: descriptor.host_alias,
                        user: descriptor.user,
                    });
                    match started {
                        Ok(_) => SessionAudit {
                            client: Some(client),
                            audit_session_id,
                        },
                        Err(error) => {
                            eprintln!("xshelld audit warning: {error:#}");
                            SessionAudit {
                                client: None,
                                audit_session_id: String::new(),
                            }
                        }
                    }
                }
                Err(error) => {
                    eprintln!("xshelld audit warning: {error:#}");
                    SessionAudit {
                        client: None,
                        audit_session_id: String::new(),
                    }
                }
            };
            sessions.insert(session_id.to_owned(), entry);
        }
        SessionAuditHandle {
            inner: Some(Arc::clone(inner)),
            session_id: session_id.to_owned(),
        }
    }

    /// The audit-service session ID for an xshell session, if one is open.
    pub fn audit_session_id(&self, session_id: &str) -> Option<String> {
        let inner = self.inner.as_ref()?;
        let sessions = inner
            .sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        sessions
            .get(session_id)
            .filter(|entry| entry.client.is_some())
            .map(|entry| entry.audit_session_id.clone())
    }

    /// Close the audit-service session for a deleted xshell session.
    pub fn close_session(&self, session_id: &str, reason: &str) {
        let Some(inner) = &self.inner else {
            return;
        };
        let removed = inner
            .sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(session_id);
        if let Some(SessionAudit {
            client: Some(mut client),
            ..
        }) = removed
        {
            let _ = client.append(AuditEvent::SessionEnded {
                reason: reason.into(),
            });
            if let Err(error) = client.close() {
                eprintln!("xshelld audit warning: could not finalize audit: {error:#}");
            }
        }
    }
}

impl DaemonAudit {
    /// Finalize every open audit session with a signed checkpoint. Called on
    /// daemon shutdown so a clean stop does not leave verifiable-but-open
    /// chains behind.
    pub fn close_all(&self, reason: &str) {
        let Some(inner) = &self.inner else {
            return;
        };
        let drained: Vec<_> = inner
            .sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .drain()
            .collect();
        for (_, entry) in drained {
            if let Some(mut client) = entry.client {
                let _ = client.append(AuditEvent::SessionEnded {
                    reason: reason.into(),
                });
                if let Err(error) = client.close() {
                    eprintln!("xshelld audit warning: could not finalize audit: {error:#}");
                }
            }
        }
    }
}

/// Identity fields recorded when the daemon first audits a session.
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
    session_id: String,
}

impl SessionAuditHandle {
    fn disabled() -> Self {
        Self {
            inner: None,
            session_id: String::new(),
        }
    }

    /// Append one event. With `required = true` an append failure is returned
    /// so the caller can refuse to continue the action; otherwise the failure
    /// is reported once and auditing for this session is switched off.
    pub fn append(&self, event: AuditEvent) -> Result<()> {
        let Some(inner) = &self.inner else {
            return Ok(());
        };
        let mut sessions = inner
            .sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Some(entry) = sessions.get_mut(&self.session_id) else {
            return Ok(());
        };
        let Some(client) = entry.client.as_mut() else {
            if inner.config.required {
                anyhow::bail!("required auditing is unavailable for this session");
            }
            return Ok(());
        };
        match client.append(event) {
            Ok(_) => Ok(()),
            Err(error) => {
                entry.client = None;
                if inner.config.required {
                    Err(error).context(
                        "required audit append failed; refusing to continue the audited action",
                    )
                } else {
                    eprintln!(
                        "xshelld audit warning: {error:#}; auditing is disabled for this session"
                    );
                    Ok(())
                }
            }
        }
    }
}

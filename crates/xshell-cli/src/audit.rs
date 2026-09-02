use anyhow::{Context, Result, bail};
use xshell_audit::{AuditClient, AuditConfig, AuditEvent};

pub struct AuditRuntime {
    client: Option<AuditClient>,
    required: bool,
    /// Whether this client records execution events (input, model output,
    /// tool calls, direct shell completion). When a session daemon owns
    /// execution it records those at the execution boundary, and the client
    /// records only what the daemon cannot see: UI attach/detach, model
    /// profile switches, view operations, and PTY terminal jobs.
    execution_events: bool,
}

impl AuditRuntime {
    pub fn start(config: &AuditConfig) -> Result<Self> {
        if !config.enabled {
            return Ok(Self {
                client: None,
                required: config.required,
                execution_events: true,
            });
        }
        let socket = config
            .socket
            .as_deref()
            .context("audit.socket is required when auditing is enabled")?;
        match AuditClient::connect(socket, env!("CARGO_PKG_VERSION")) {
            Ok(client) => Ok(Self {
                client: Some(client),
                required: config.required,
                execution_events: true,
            }),
            Err(error) if config.required => Err(error).context(
                "required auditing is unavailable; xshell will not start without an audit trail",
            ),
            Err(error) => {
                eprintln!("xshell audit warning: {error:#}; auditing is disabled for this session");
                Ok(Self {
                    client: None,
                    required: false,
                    execution_events: true,
                })
            }
        }
    }

    /// Hand execution-event recording over to a session daemon. Call this
    /// when the session fabric is enabled; the daemon audits turns itself.
    pub fn delegate_execution_events(&mut self) {
        self.execution_events = false;
    }

    /// Append an execution event. A no-op when a daemon owns execution, so
    /// replayed daemon events are never recorded twice.
    pub fn append_execution(&mut self, event: AuditEvent) -> Result<()> {
        if !self.execution_events {
            return Ok(());
        }
        self.append(event)
    }

    pub fn append(&mut self, event: AuditEvent) -> Result<()> {
        let Some(client) = self.client.as_mut() else {
            return Ok(());
        };
        if let Err(error) = client.append(event) {
            if self.required {
                return Err(error).context(
                    "required audit append failed; refusing to continue the audited action",
                );
            }
            eprintln!("xshell audit warning: {error:#}; auditing is disabled for this session");
            self.client = None;
        }
        Ok(())
    }

    pub fn session_id(&self) -> Option<&str> {
        self.client.as_ref().map(AuditClient::session_id)
    }

    pub fn signing_key_id(&self) -> Option<&str> {
        self.client.as_ref().map(AuditClient::signing_key_id)
    }

    pub fn close(mut self, reason: &str) -> Result<()> {
        let Some(mut client) = self.client.take() else {
            return Ok(());
        };
        let result = (|| {
            client.append(AuditEvent::SessionEnded {
                reason: reason.into(),
            })?;
            let checkpoint = client.close()?;
            if !checkpoint.body.final_checkpoint {
                bail!("audit service did not return a final checkpoint");
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(()),
            Err(error) if self.required => Err(error).context("could not finalize required audit"),
            Err(error) => {
                eprintln!("xshell audit warning: could not finalize audit: {error:#}");
                Ok(())
            }
        }
    }
}

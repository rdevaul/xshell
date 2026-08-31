use anyhow::{Context, Result, bail};
use xshell_audit::{AuditClient, AuditConfig, AuditEvent};

pub struct AuditRuntime {
    client: Option<AuditClient>,
    required: bool,
}

impl AuditRuntime {
    pub fn start(config: &AuditConfig) -> Result<Self> {
        if !config.enabled {
            return Ok(Self {
                client: None,
                required: config.required,
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
            }),
            Err(error) if config.required => Err(error).context(
                "required auditing is unavailable; xshell will not start without an audit trail",
            ),
            Err(error) => {
                eprintln!("xshell audit warning: {error:#}; auditing is disabled for this session");
                Ok(Self {
                    client: None,
                    required: false,
                })
            }
        }
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

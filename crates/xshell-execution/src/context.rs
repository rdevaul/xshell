//! Turn context: system-prompt text that trusted, compiled-in code supplies
//! for each agent turn.
//!
//! A session's system prompt is fixed when the session is created. A
//! [`ContextProvider`] adds text that is rebuilt at the start of every turn,
//! such as an assistant's identity, a user's memory, or an index of skills.
//! The text is appended to the system prompt of that turn's provider
//! requests and is never written to the session's history, so the next turn
//! gets a fresh one and old context never accumulates.
//!
//! Like a tool provider, a context provider is registered by the binary that
//! builds the engine; nothing is discovered at run time. Its text has the
//! standing of the system prompt, so what it contains is the provider's
//! responsibility. See `docs/tool-providers.md`.

use crate::engine::CancellationFlag;
use anyhow::{Result, bail};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Longest context one turn may carry. The text is sent with every provider
/// request of the turn, so this bounds that standing cost.
pub const MAX_TURN_CONTEXT_BYTES: usize = 256 * 1024;
/// Longest provider name, as for tool providers.
const MAX_NAME_BYTES: usize = 64;

/// What a provider is told about the turn it supplies context for.
#[derive(Debug, Clone)]
pub struct ContextRequest {
    /// The session the turn belongs to.
    pub session_id: String,
    /// The session's name, unique on its host.
    pub session_name: String,
    /// The turn's id, as recorded in events and audit.
    pub turn_id: String,
    /// The session's working directory.
    pub cwd: PathBuf,
    /// The message that starts the turn, so a provider can select what is
    /// relevant to it.
    pub message: String,
    /// Set when the turn is cancelled. The caller also stops awaiting the
    /// provider then.
    pub cancellation: CancellationFlag,
}

/// Trusted code that supplies system-prompt text for each agent turn.
///
/// [`context`](Self::context) runs once, before the turn's first provider
/// request, on the turn's async runtime. It is bounded by a deadline and
/// dropped when the turn is cancelled, so it must be cancellation-safe and
/// must not block the thread. An `Err`, a panic, a timeout, or text over
/// [`MAX_TURN_CONTEXT_BYTES`] fails the turn before anything runs: an agent
/// without its identity or its user's memory should not act. Return
/// `Ok(None)` for a turn that needs no context.
#[async_trait]
pub trait ContextProvider: Send + Sync {
    /// A short name for this provider, recorded in audit records.
    fn name(&self) -> &str;

    /// The text to append to the system prompt for this turn.
    async fn context(&self, request: ContextRequest) -> Result<Option<String>>;
}

/// One turn's context: the text, who supplied it, and its hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnContext {
    provider: String,
    text: String,
    sha256: String,
}

impl TurnContext {
    /// Check `text` and take its hash. Empty or whitespace-only text is not
    /// a context; text over [`MAX_TURN_CONTEXT_BYTES`] is refused, not
    /// truncated, since a cut-off instruction can mean something else.
    pub fn new(provider: &str, text: String) -> Result<Option<Self>> {
        validate_provider_name(provider)?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        if text.len() > MAX_TURN_CONTEXT_BYTES {
            bail!(
                "context provider {provider:?} returned {} bytes, over the limit of \
{MAX_TURN_CONTEXT_BYTES}",
                text.len()
            );
        }
        let sha256 = hex::encode(Sha256::digest(text.as_bytes()));
        Ok(Some(Self {
            provider: provider.to_owned(),
            text,
            sha256,
        }))
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Lowercase-hex SHA-256 of the text's UTF-8 bytes.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn bytes(&self) -> usize {
        self.text.len()
    }
}

fn validate_provider_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let valid = name.len() <= MAX_NAME_BYTES
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !valid {
        bail!(
            "context provider name {name:?} must be 1 to {MAX_NAME_BYTES} characters of a-z, \
0-9, and _, starting with a letter"
        );
    }
    Ok(())
}

/// Ask `provider` for a turn's context, within `deadline`.
///
/// The provider runs as its own task, so a panic is an error here instead of
/// unwinding the turn's thread, and the task is aborted on timeout or
/// cancellation.
pub async fn resolve_turn_context(
    provider: Arc<dyn ContextProvider>,
    request: ContextRequest,
    deadline: Duration,
) -> Result<Option<TurnContext>> {
    let name = provider.name().to_owned();
    validate_provider_name(&name)?;
    let cancellation = request.cancellation.clone();
    let mut task = tokio::spawn(async move { provider.context(request).await });
    let joined = tokio::select! {
        joined = tokio::time::timeout(deadline, &mut task) => joined,
        () = cancellation.wait() => {
            task.abort();
            bail!("context provider {name:?} was cancelled with the agent turn");
        }
    };
    match joined {
        Ok(Ok(Ok(Some(text)))) => TurnContext::new(&name, text),
        Ok(Ok(Ok(None))) => Ok(None),
        Ok(Ok(Err(error))) => Err(error.context(format!("context provider {name:?} failed"))),
        Ok(Err(join_error)) if join_error.is_panic() => {
            bail!("context provider {name:?} failed unexpectedly (it panicked)")
        }
        Ok(Err(_)) => bail!("context provider {name:?} was stopped"),
        Err(_elapsed) => {
            task.abort();
            bail!(
                "context provider {name:?} did not answer within {} s",
                deadline.as_secs()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scripted(&'static str);

    #[async_trait]
    impl ContextProvider for Scripted {
        fn name(&self) -> &str {
            "identity"
        }

        async fn context(&self, request: ContextRequest) -> Result<Option<String>> {
            match self.0 {
                "fail" => bail!("memory is unreadable"),
                "panic" => panic!("provider bug"),
                "hang" => {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
                "none" => Ok(None),
                "blank" => Ok(Some("  \n".into())),
                "huge" => Ok(Some("x".repeat(MAX_TURN_CONTEXT_BYTES + 1))),
                _ => Ok(Some(format!(
                    "You are talking to {} about {:?}.",
                    request.session_name, request.message
                ))),
            }
        }
    }

    fn request(cancellation: CancellationFlag) -> ContextRequest {
        ContextRequest {
            session_id: "s1".into(),
            session_name: "alice".into(),
            turn_id: "t1".into(),
            cwd: "/".into(),
            message: "lunch".into(),
            cancellation,
        }
    }

    async fn resolve(script: &'static str, deadline: Duration) -> Result<Option<TurnContext>> {
        resolve_turn_context(
            Arc::new(Scripted(script)),
            request(CancellationFlag::default()),
            deadline,
        )
        .await
    }

    #[tokio::test]
    async fn a_provider_sees_the_turn_and_its_text_is_hashed() {
        let context = resolve("ok", Duration::from_secs(30))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(context.provider(), "identity");
        assert_eq!(context.text(), "You are talking to alice about \"lunch\".");
        assert_eq!(context.bytes(), context.text().len());
        assert_eq!(
            context.sha256(),
            hex::encode(Sha256::digest(context.text().as_bytes()))
        );
    }

    #[tokio::test]
    async fn no_text_and_blank_text_are_no_context() {
        let long = Duration::from_secs(30);
        assert_eq!(resolve("none", long).await.unwrap(), None);
        assert_eq!(resolve("blank", long).await.unwrap(), None);
    }

    #[tokio::test]
    async fn errors_panics_timeouts_and_oversize_text_are_errors() {
        let long = Duration::from_secs(30);
        for (script, deadline, message) in [
            ("fail", long, "memory is unreadable"),
            ("panic", long, "it panicked"),
            ("hang", Duration::from_millis(50), "did not answer"),
            ("huge", long, "over the limit"),
        ] {
            let error = resolve(script, deadline).await.unwrap_err();
            assert!(
                format!("{error:#}").contains(message),
                "{script}: {error:#}"
            );
        }
    }

    #[tokio::test]
    async fn cancelling_the_turn_stops_a_hung_provider() {
        let cancellation = CancellationFlag::default();
        let canceller = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            canceller.cancel();
        });
        let error = resolve_turn_context(
            Arc::new(Scripted("hang")),
            request(cancellation),
            Duration::from_secs(60),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"), "{error:#}");
    }

    #[test]
    fn provider_names_follow_the_tool_provider_shape() {
        assert!(TurnContext::new("identity", "x".into()).unwrap().is_some());
        for bad in ["", "Identity", "my-context", "9lives"] {
            assert!(TurnContext::new(bad, "x".into()).is_err(), "{bad:?}");
        }
    }
}

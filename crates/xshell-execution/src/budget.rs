use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The longest duration a limit may be configured with. Keeps deadline
/// arithmetic far from overflow; a turn that needs longer should be split.
pub const MAX_LIMIT_DURATION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Limits on the work one root turn may do, across every agent it starts.
///
/// The provider-round limit alone does not bound a turn: one response may
/// carry hundreds of tool calls, and agents that start other agents multiply
/// any per-agent limit. These limits are drawn from one shared
/// [`TurnBudget`], so they bound the root turn as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnLimits {
    /// Provider requests, including automatic continuations and the
    /// context-overflow retry.
    pub provider_requests: usize,
    /// Tool calls evaluated, whether approved, denied, or refused. Denied
    /// calls count because each can cost a human decision.
    pub tool_calls: usize,
    /// Tool calls evaluated from a single response. Calls beyond this are
    /// refused without evaluation.
    pub tool_calls_per_response: usize,
    /// Elapsed time, checked before every provider request and tool call and
    /// enforced during provider requests. It is not checked while a tool
    /// runs; a running `run_shell` call is bounded by `shell_timeout` plus a
    /// fixed kill-and-drain allowance of a few seconds (background jobs
    /// included, see F17), so a turn overruns by at most about one shell
    /// timeout. Other tools carry their own bounds.
    #[serde(with = "seconds")]
    pub wall_time: Duration,
    /// How long one `run_shell` tool call may run before its process group
    /// is killed.
    #[serde(with = "seconds")]
    pub shell_timeout: Duration,
    /// Automatic continuations per agent, after a response that ended
    /// without finishing (for example, cut off by the output token limit).
    pub auto_continues: usize,
    /// Bytes of tool results the turn may hand back to the model, summed
    /// over every agent in the turn. The result that crosses it is kept;
    /// the turn then stops at its next provider request or tool call.
    pub tool_output_bytes: usize,
}

/// One mebibyte, for the byte limits below.
const MIB: usize = 1024 * 1024;

impl Default for TurnLimits {
    fn default() -> Self {
        Self {
            provider_requests: 128,
            tool_calls: 256,
            tool_calls_per_response: 64,
            wall_time: Duration::from_secs(60 * 60),
            shell_timeout: Duration::from_secs(60),
            auto_continues: 2,
            tool_output_bytes: 8 * MIB,
        }
    }
}

impl TurnLimits {
    /// The built-in `relaxed` profile for long engineering tasks: elapsed
    /// time is the governing limit, and the counts are high but finite, so a
    /// runaway loop still stops deterministically.
    pub fn relaxed() -> Self {
        Self {
            provider_requests: 2_000,
            tool_calls: 5_000,
            tool_calls_per_response: 128,
            wall_time: Duration::from_secs(8 * 60 * 60),
            shell_timeout: Duration::from_secs(30 * 60),
            auto_continues: 50,
            tool_output_bytes: 128 * MIB,
        }
    }

    /// The built-in profile with this name, if there is one.
    pub fn builtin(name: &str) -> Option<Self> {
        match name {
            "default" => Some(Self::default()),
            "relaxed" => Some(Self::relaxed()),
            _ => None,
        }
    }

    /// Each limit lowered to `ceiling`'s where it exceeds it. Returns the
    /// result and whether anything was lowered.
    pub fn clamp_to(self, ceiling: TurnLimits) -> (Self, bool) {
        let clamped = Self {
            provider_requests: self.provider_requests.min(ceiling.provider_requests),
            tool_calls: self.tool_calls.min(ceiling.tool_calls),
            tool_calls_per_response: self
                .tool_calls_per_response
                .min(ceiling.tool_calls_per_response),
            wall_time: self.wall_time.min(ceiling.wall_time),
            shell_timeout: self.shell_timeout.min(ceiling.shell_timeout),
            auto_continues: self.auto_continues.min(ceiling.auto_continues),
            tool_output_bytes: self.tool_output_bytes.min(ceiling.tool_output_bytes),
        };
        (clamped, clamped != self)
    }

    /// Check that every limit can be enforced: counts at least one (zero
    /// automatic continuations is allowed) and durations between one second
    /// and [`MAX_LIMIT_DURATION`].
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("provider_requests", self.provider_requests),
            ("tool_calls", self.tool_calls),
            ("tool_calls_per_response", self.tool_calls_per_response),
            ("tool_output_bytes", self.tool_output_bytes),
        ] {
            if value == 0 {
                bail!("{name} must be at least 1");
            }
        }
        for (name, value) in [
            ("wall_time", self.wall_time),
            ("shell_timeout", self.shell_timeout),
        ] {
            if value < Duration::from_secs(1) || value > MAX_LIMIT_DURATION {
                bail!(
                    "{name} must be between 1 second and {} days",
                    MAX_LIMIT_DURATION.as_secs() / 86_400
                );
            }
        }
        Ok(())
    }

    /// One line describing the limits, for status output.
    pub fn summary(&self) -> String {
        format!(
            "{} provider requests, {} tool calls ({} per response), {} wall time, \
{} shell timeout, {} automatic continuations, {} of tool output",
            self.provider_requests,
            self.tool_calls,
            self.tool_calls_per_response,
            format_span(self.wall_time),
            format_span(self.shell_timeout),
            self.auto_continues,
            format_bytes(self.tool_output_bytes),
        )
    }
}

/// A set of limits as written in configuration: any field may be left out.
/// Durations are written as a number with a unit, such as `"90s"`, `"30m"`,
/// or `"8h"`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_requests: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls_per_response: Option<usize>,
    #[serde(
        deserialize_with = "optional_duration",
        serialize_with = "optional_span",
        skip_serializing_if = "Option::is_none"
    )]
    pub wall_time: Option<Duration>,
    #[serde(
        deserialize_with = "optional_duration",
        serialize_with = "optional_span",
        skip_serializing_if = "Option::is_none"
    )]
    pub shell_timeout: Option<Duration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_continues: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_output_bytes: Option<usize>,
}

impl LimitSettings {
    /// `base` with the fields set here replaced, checked with
    /// [`TurnLimits::validate`].
    pub fn apply_to(&self, base: TurnLimits) -> Result<TurnLimits> {
        let limits = TurnLimits {
            provider_requests: self.provider_requests.unwrap_or(base.provider_requests),
            tool_calls: self.tool_calls.unwrap_or(base.tool_calls),
            tool_calls_per_response: self
                .tool_calls_per_response
                .unwrap_or(base.tool_calls_per_response),
            wall_time: self.wall_time.unwrap_or(base.wall_time),
            shell_timeout: self.shell_timeout.unwrap_or(base.shell_timeout),
            auto_continues: self.auto_continues.unwrap_or(base.auto_continues),
            tool_output_bytes: self.tool_output_bytes.unwrap_or(base.tool_output_bytes),
        };
        limits.validate()?;
        Ok(limits)
    }
}

/// Parse a duration such as `"45s"`, `"30m"`, `"8h"`, or `"1h30m"`.
pub fn parse_duration(text: &str) -> Result<Duration> {
    let text = text.trim();
    if text.is_empty() {
        bail!("a duration cannot be empty");
    }
    let mut total = Duration::ZERO;
    let mut digits = String::new();
    for character in text.chars() {
        if character.is_ascii_digit() {
            digits.push(character);
            continue;
        }
        let unit = match character {
            's' => 1,
            'm' => 60,
            'h' => 60 * 60,
            'd' => 24 * 60 * 60,
            _ => bail!(
                "{text:?} is not a duration; use a number with s, m, h, or d, such as \"30m\""
            ),
        };
        let value: u64 = digits
            .parse()
            .with_context(|| format!("{text:?} is not a duration; each unit needs a number"))?;
        digits.clear();
        total = value
            .checked_mul(unit)
            .and_then(|seconds| total.checked_add(Duration::from_secs(seconds)))
            .with_context(|| format!("{text:?} is too long"))?;
    }
    if !digits.is_empty() {
        bail!("{text:?} needs a unit after its last number: s, m, h, or d");
    }
    Ok(total)
}

fn optional_duration<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Duration>, D::Error> {
    let text = String::deserialize(deserializer)?;
    parse_duration(&text)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

fn optional_span<S: Serializer>(
    value: &Option<Duration>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    match value {
        Some(duration) => serializer.serialize_str(&format_span(*duration)),
        None => serializer.serialize_none(),
    }
}

/// Durations travel between client and daemon as whole seconds.
mod seconds {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(value.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        u64::deserialize(deserializer).map(Duration::from_secs)
    }
}

/// Which limit a turn exhausted. Carried as the turn's error so callers (and
/// a parent agent, for a child turn) can tell budget exhaustion from failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BudgetExhausted {
    #[error("agent turn exhausted its budget of {0} provider requests")]
    ProviderRequests(usize),
    #[error("agent turn exhausted its budget of {0} tool calls")]
    ToolCalls(usize),
    #[error("agent turn exhausted its {} time budget", format_duration(*.0))]
    WallTime(Duration),
    #[error("agent turn exhausted its budget of {} of tool output", format_bytes(*.0))]
    ToolOutputBytes(usize),
}

/// The shared, consumable form of [`TurnLimits`] for one root turn. Clones
/// share counters, so a child agent given a clone draws from its parent's
/// budget rather than receiving a fresh one.
#[derive(Debug, Clone)]
pub struct TurnBudget(Arc<BudgetInner>);

#[derive(Debug)]
struct BudgetInner {
    limits: TurnLimits,
    started: Instant,
    deadline: Instant,
    provider_requests: AtomicUsize,
    tool_calls: AtomicUsize,
    tool_output_bytes: AtomicUsize,
}

impl TurnBudget {
    pub fn new(limits: TurnLimits) -> Self {
        let now = Instant::now();
        // A wall time beyond what the clock can represent means "no practical
        // deadline"; validated limits never get near it.
        let deadline = now
            .checked_add(limits.wall_time)
            .unwrap_or_else(|| now + MAX_LIMIT_DURATION);
        Self(Arc::new(BudgetInner {
            limits,
            started: now,
            deadline,
            provider_requests: AtomicUsize::new(0),
            tool_calls: AtomicUsize::new(0),
            tool_output_bytes: AtomicUsize::new(0),
        }))
    }

    pub fn limits(&self) -> TurnLimits {
        self.0.limits
    }

    /// Reserve one provider request, or report the limit that prevents it.
    pub fn charge_provider_request(&self) -> Result<(), BudgetExhausted> {
        self.check_time()?;
        self.check_output()?;
        charge(
            &self.0.provider_requests,
            self.0.limits.provider_requests,
            BudgetExhausted::ProviderRequests,
        )
    }

    /// Reserve one tool call, or report the limit that prevents it.
    pub fn charge_tool_call(&self) -> Result<(), BudgetExhausted> {
        self.check_time()?;
        self.check_output()?;
        charge(
            &self.0.tool_calls,
            self.0.limits.tool_calls,
            BudgetExhausted::ToolCalls,
        )
    }

    pub fn check_time(&self) -> Result<(), BudgetExhausted> {
        if Instant::now() >= self.0.deadline {
            Err(BudgetExhausted::WallTime(self.0.limits.wall_time))
        } else {
            Ok(())
        }
    }

    /// Count `bytes` of tool result handed back to the model. Never fails:
    /// the result is already produced, and keeping it keeps the history
    /// consistent. The next charge reports the exhaustion.
    pub fn record_tool_output(&self, bytes: usize) {
        let _ =
            self.0
                .tool_output_bytes
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                    Some(used.saturating_add(bytes))
                });
    }

    fn check_output(&self) -> Result<(), BudgetExhausted> {
        let limit = self.0.limits.tool_output_bytes;
        if self.0.tool_output_bytes.load(Ordering::Acquire) >= limit {
            Err(BudgetExhausted::ToolOutputBytes(limit))
        } else {
            Ok(())
        }
    }

    pub fn tool_output_bytes_used(&self) -> usize {
        self.0.tool_output_bytes.load(Ordering::Acquire)
    }

    /// What the turn has used so far, against each limit.
    pub fn usage(&self) -> TurnUsage {
        let limits = self.0.limits;
        let count = |used: usize, limit: usize| UsageMeasure::enforced(used as u64, limit as u64);
        TurnUsage {
            model_rounds: count(self.provider_requests_used(), limits.provider_requests),
            tool_dispatches: count(self.tool_calls_used(), limits.tool_calls),
            tool_output_bytes: count(self.tool_output_bytes_used(), limits.tool_output_bytes),
            wall_time_ms: UsageMeasure::enforced(
                duration_ms(self.0.started.elapsed()),
                duration_ms(limits.wall_time),
            ),
            tokens: UsageMeasure::unavailable(),
        }
    }

    /// Resolves when the time budget runs out.
    pub async fn expired(&self) {
        tokio::time::sleep_until(self.0.deadline.into()).await;
    }

    pub fn provider_requests_used(&self) -> usize {
        self.0.provider_requests.load(Ordering::Acquire)
    }

    pub fn tool_calls_used(&self) -> usize {
        self.0.tool_calls.load(Ordering::Acquire)
    }
}

/// How much of each limit a turn used, in FutureShell's autonomy-budget
/// vocabulary: provider requests are *model rounds* and evaluated tool calls
/// are *tool dispatches*. Each measure says how far it can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnUsage {
    pub model_rounds: UsageMeasure,
    pub tool_dispatches: UsageMeasure,
    pub tool_output_bytes: UsageMeasure,
    pub wall_time_ms: UsageMeasure,
    /// Providers' token counts are not collected yet.
    pub tokens: UsageMeasure,
}

/// One measure of use against its limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageMeasure {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    pub assurance: Assurance,
}

impl UsageMeasure {
    fn enforced(used: u64, limit: u64) -> Self {
        Self {
            used: Some(used),
            limit: Some(limit),
            assurance: Assurance::Enforced,
        }
    }

    fn unavailable() -> Self {
        Self {
            used: None,
            limit: None,
            assurance: Assurance::Unavailable,
        }
    }
}

/// How far a usage measure can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Assurance {
    /// Counted by the engine, which stopped the turn at the limit.
    Enforced,
    /// Reported by a provider, not enforced by the engine.
    Measured,
    /// Not known.
    Unavailable,
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Increment `counter` unless it has reached `limit`. A compare-exchange loop
/// rather than `fetch_add` so concurrent agents cannot overshoot the limit.
fn charge(
    counter: &AtomicUsize,
    limit: usize,
    exhausted: fn(usize) -> BudgetExhausted,
) -> Result<(), BudgetExhausted> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            (used < limit).then_some(used + 1)
        })
        .map(|_| ())
        .map_err(|_| exhausted(limit))
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3600 && seconds.is_multiple_of(3600) {
        format!("{}-hour", seconds / 3600)
    } else if seconds >= 60 && seconds.is_multiple_of(60) {
        format!("{}-minute", seconds / 60)
    } else if seconds > 0 {
        format!("{seconds}-second")
    } else {
        format!("{}-millisecond", duration.as_millis())
    }
}

/// A byte count for messages: `"8 MiB"`, `"512 KiB"`, `"100 bytes"`.
fn format_bytes(bytes: usize) -> String {
    const KIB: usize = 1024;
    if bytes >= MIB && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else if bytes >= KIB && bytes.is_multiple_of(KIB) {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{bytes} bytes")
    }
}

/// A duration as configuration writes it: `"8h"`, `"30m"`, `"90s"`.
fn format_span(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3600 && seconds.is_multiple_of(3600) {
        format!("{}h", seconds / 3600)
    } else if seconds >= 60 && seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(provider_requests: usize, tool_calls: usize) -> TurnLimits {
        TurnLimits {
            provider_requests,
            tool_calls,
            ..TurnLimits::default()
        }
    }

    #[test]
    fn usage_reports_each_limit_with_its_assurance() {
        let budget = TurnBudget::new(limits(4, 5));
        budget.charge_provider_request().unwrap();
        budget.charge_tool_call().unwrap();
        budget.charge_tool_call().unwrap();
        budget.record_tool_output(10);
        let usage = budget.usage();
        assert_eq!(usage.model_rounds.used, Some(1));
        assert_eq!(usage.model_rounds.limit, Some(4));
        assert_eq!(usage.tool_dispatches.used, Some(2));
        assert_eq!(usage.tool_output_bytes.used, Some(10));
        assert_eq!(usage.wall_time_ms.limit, Some(3_600_000));
        assert_eq!(usage.model_rounds.assurance, Assurance::Enforced);
        assert_eq!(usage.tokens.assurance, Assurance::Unavailable);
        assert_eq!(
            serde_json::to_value(usage.tokens).unwrap(),
            serde_json::json!({"assurance": "unavailable"})
        );
    }

    #[test]
    fn charges_stop_exactly_at_the_limit() {
        let budget = TurnBudget::new(limits(2, 3));
        assert!(budget.charge_provider_request().is_ok());
        assert!(budget.charge_provider_request().is_ok());
        assert_eq!(
            budget.charge_provider_request(),
            Err(BudgetExhausted::ProviderRequests(2))
        );
        for _ in 0..3 {
            budget.charge_tool_call().unwrap();
        }
        assert_eq!(
            budget.charge_tool_call(),
            Err(BudgetExhausted::ToolCalls(3))
        );
        // A refused charge does not consume anything.
        assert_eq!(budget.provider_requests_used(), 2);
        assert_eq!(budget.tool_calls_used(), 3);
    }

    #[test]
    fn clones_share_one_budget() {
        let parent = TurnBudget::new(limits(3, 1));
        let child = parent.clone();
        parent.charge_provider_request().unwrap();
        child.charge_provider_request().unwrap();
        child.charge_tool_call().unwrap();
        assert_eq!(parent.provider_requests_used(), 2);
        assert_eq!(
            parent.charge_tool_call(),
            Err(BudgetExhausted::ToolCalls(1))
        );
    }

    #[test]
    fn concurrent_charges_never_overshoot() {
        let budget = TurnBudget::new(limits(1, 100));
        let granted: usize = std::thread::scope(|scope| {
            (0..8)
                .map(|_| {
                    let budget = budget.clone();
                    scope.spawn(move || {
                        (0..50)
                            .filter(|_| budget.charge_tool_call().is_ok())
                            .count()
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .sum()
        });
        assert_eq!(granted, 100);
        assert_eq!(budget.tool_calls_used(), 100);
    }

    #[test]
    fn an_expired_budget_refuses_everything() {
        let budget = TurnBudget::new(TurnLimits {
            wall_time: Duration::ZERO,
            ..TurnLimits::default()
        });
        let expired = Err(BudgetExhausted::WallTime(Duration::ZERO));
        assert_eq!(budget.check_time(), expired);
        assert_eq!(budget.charge_provider_request(), expired);
        assert_eq!(budget.charge_tool_call(), expired);
        assert_eq!(budget.provider_requests_used(), 0);
    }

    #[test]
    fn exhaustion_messages_name_the_limit() {
        assert_eq!(
            BudgetExhausted::WallTime(Duration::from_secs(3600)).to_string(),
            "agent turn exhausted its 1-hour time budget"
        );
        assert_eq!(
            BudgetExhausted::WallTime(Duration::from_secs(90)).to_string(),
            "agent turn exhausted its 90-second time budget"
        );
        assert_eq!(
            BudgetExhausted::ToolCalls(128).to_string(),
            "agent turn exhausted its budget of 128 tool calls"
        );
    }

    #[test]
    fn durations_parse_with_units() {
        assert_eq!(parse_duration("45s").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("8h").unwrap(), Duration::from_secs(8 * 3600));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(
            parse_duration("2d").unwrap(),
            Duration::from_secs(2 * 86_400)
        );
        for bad in ["", "30", "m", "5x", "1h30", "99999999999999999999h"] {
            assert!(parse_duration(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn settings_override_only_the_fields_they_set_and_are_validated() {
        let settings: LimitSettings = toml_like(r#"{"tool_calls": 900, "wall_time": "2h"}"#);
        let limits = settings.apply_to(TurnLimits::default()).unwrap();
        assert_eq!(limits.tool_calls, 900);
        assert_eq!(limits.wall_time, Duration::from_secs(7200));
        assert_eq!(
            limits.provider_requests,
            TurnLimits::default().provider_requests
        );

        let zero: LimitSettings = toml_like(r#"{"tool_calls": 0}"#);
        assert!(zero.apply_to(TurnLimits::default()).is_err());
        let too_long: LimitSettings = toml_like(r#"{"shell_timeout": "31d"}"#);
        assert!(too_long.apply_to(TurnLimits::default()).is_err());
        let no_continues: LimitSettings = toml_like(r#"{"auto_continues": 0}"#);
        assert_eq!(
            no_continues
                .apply_to(TurnLimits::default())
                .unwrap()
                .auto_continues,
            0
        );
    }

    fn toml_like(json: &str) -> LimitSettings {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn clamping_lowers_each_limit_independently() {
        let ceiling = TurnLimits {
            tool_calls: 1_000,
            wall_time: Duration::from_secs(3600),
            ..TurnLimits::relaxed()
        };
        let (clamped, lowered) = TurnLimits::relaxed().clamp_to(ceiling);
        assert!(lowered);
        assert_eq!(clamped.tool_calls, 1_000);
        assert_eq!(clamped.wall_time, Duration::from_secs(3600));
        assert_eq!(
            clamped.provider_requests,
            TurnLimits::relaxed().provider_requests
        );
        let (same, lowered) = TurnLimits::default().clamp_to(TurnLimits::relaxed());
        assert!(!lowered);
        assert_eq!(same, TurnLimits::default());
    }

    #[test]
    fn limits_travel_as_whole_seconds() {
        let json = serde_json::to_value(TurnLimits::relaxed()).unwrap();
        assert_eq!(json["wall_time"], 8 * 3600);
        assert_eq!(json["shell_timeout"], 1800);
        let back: TurnLimits = serde_json::from_value(json).unwrap();
        assert_eq!(back, TurnLimits::relaxed());
    }

    #[test]
    fn the_relaxed_profile_is_valid_and_larger_than_the_default() {
        let relaxed = TurnLimits::relaxed();
        relaxed.validate().unwrap();
        TurnLimits::default().validate().unwrap();
        assert!(!TurnLimits::default().clamp_to(relaxed).1);
        assert_eq!(TurnLimits::builtin("relaxed"), Some(relaxed));
        assert_eq!(TurnLimits::builtin("nope"), None);
    }
}

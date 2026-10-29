use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Limits on the work one root turn may do, across every agent it starts.
///
/// The provider-round limit alone does not bound a turn: one response may
/// carry hundreds of tool calls, and agents that start other agents multiply
/// any per-agent limit. These limits are drawn from one shared
/// [`TurnBudget`], so they bound the root turn as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// enforced during provider requests. A running tool is bounded by its
    /// own timeout, so a turn can overrun by at most one tool's timeout.
    pub wall_time: Duration,
}

impl Default for TurnLimits {
    fn default() -> Self {
        Self {
            provider_requests: 64,
            tool_calls: 128,
            tool_calls_per_response: 32,
            wall_time: Duration::from_secs(60 * 60),
        }
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
}

/// The shared, consumable form of [`TurnLimits`] for one root turn. Clones
/// share counters, so a child agent given a clone draws from its parent's
/// budget rather than receiving a fresh one.
#[derive(Debug, Clone)]
pub struct TurnBudget(Arc<BudgetInner>);

#[derive(Debug)]
struct BudgetInner {
    limits: TurnLimits,
    deadline: Instant,
    provider_requests: AtomicUsize,
    tool_calls: AtomicUsize,
}

impl TurnBudget {
    pub fn new(limits: TurnLimits) -> Self {
        Self(Arc::new(BudgetInner {
            limits,
            deadline: Instant::now() + limits.wall_time,
            provider_requests: AtomicUsize::new(0),
            tool_calls: AtomicUsize::new(0),
        }))
    }

    pub fn limits(&self) -> TurnLimits {
        self.0.limits
    }

    /// Reserve one provider request, or report the limit that prevents it.
    pub fn charge_provider_request(&self) -> Result<(), BudgetExhausted> {
        self.check_time()?;
        charge(
            &self.0.provider_requests,
            self.0.limits.provider_requests,
            BudgetExhausted::ProviderRequests,
        )
    }

    /// Reserve one tool call, or report the limit that prevents it.
    pub fn charge_tool_call(&self) -> Result<(), BudgetExhausted> {
        self.check_time()?;
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
    if seconds >= 60 && seconds.is_multiple_of(60) {
        format!("{}-minute", seconds / 60)
    } else if seconds > 0 {
        format!("{seconds}-second")
    } else {
        format!("{}-millisecond", duration.as_millis())
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
            "agent turn exhausted its 60-minute time budget"
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
}

//! Immutable checked-plan prototype for FutureShell Flow IR.

mod conflict;
mod hash;
mod lower;
mod model;

pub use hash::{PlanHash, canonical_plan_bytes};
pub use lower::{PlanDiagnostic, PlanError, lower};
pub use model::*;

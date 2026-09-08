//! Immutable checked-plan and pure catalog-resolution prototype for FutureShell Flow IR.

mod conflict;
mod hash;
mod lower;
mod model;
mod resolve;

pub use hash::{PlanHash, canonical_plan_bytes};
pub use lower::{PlanDiagnostic, PlanError, lower};
pub use model::*;
pub use resolve::*;

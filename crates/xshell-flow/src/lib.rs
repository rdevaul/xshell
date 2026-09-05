//! Prototype typed dataflow representation for FutureShell programs.
//!
//! `Flow` is an authoring IR shared by textual and graphical frontends. It is
//! deliberately distinct from the immutable, authorized execution plan.

mod dot;
mod hash;
mod model;
mod validate;

pub use dot::to_dot;
pub use hash::{HashError, SemanticHash, canonical_semantic_bytes, semantic_hash};
pub use model::*;
pub use validate::{Diagnostic, ValidationError, validate};

impl Flow {
    /// Validate this flow and return every independently detectable problem.
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate(self)
    }

    /// Hash the semantic graph, excluding presentation metadata and ordering.
    pub fn semantic_hash(&self) -> Result<SemanticHash, HashError> {
        semantic_hash(self)
    }
}

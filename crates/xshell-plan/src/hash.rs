use crate::Plan;
use sha2::{Digest, Sha256};
use std::fmt;

const PLAN_HASH_DOMAIN: &[u8] = b"xshell.plan.semantic.v0\0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanHash([u8; 32]);

impl PlanHash {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for PlanHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex::encode(self.0))
    }
}

pub fn canonical_plan_bytes(plan: &Plan) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(plan)
}

pub(crate) fn plan_hash(plan: &Plan) -> Result<PlanHash, serde_json::Error> {
    let bytes = canonical_plan_bytes(plan)?;
    let mut hasher = Sha256::new();
    hasher.update(PLAN_HASH_DOMAIN);
    hasher.update(bytes);
    Ok(PlanHash(hasher.finalize().into()))
}

use crate::*;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use thiserror::Error;

const CANONICAL_MAGIC: &[u8; 8] = b"FSPLAN\0\x01";
const PLAN_HASH_DOMAIN: &[u8] = b"xshell.plan.semantic.fsplan-v1\0";

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

#[derive(Debug, Error)]
pub enum CanonicalEncodeError {
    #[error("unsupported plan schema {actual:?}; expected {expected:?}")]
    UnsupportedSchema {
        actual: String,
        expected: &'static str,
    },
    #[error("canonical value exceeds the u32 length limit")]
    LengthOverflow,
    #[error("JSON number cannot be represented canonically")]
    UnsupportedNumber,
}

pub fn canonical_plan_bytes(plan: &Plan) -> Result<Vec<u8>, CanonicalEncodeError> {
    if plan.schema != PLAN_SCHEMA_V0 {
        return Err(CanonicalEncodeError::UnsupportedSchema {
            actual: plan.schema.clone(),
            expected: PLAN_SCHEMA_V0,
        });
    }
    let mut encoder = Encoder::default();
    encoder.bytes.extend_from_slice(CANONICAL_MAGIC);
    plan.encode(&mut encoder)?;
    Ok(encoder.bytes)
}

pub(crate) fn plan_hash(plan: &Plan) -> Result<PlanHash, CanonicalEncodeError> {
    let bytes = canonical_plan_bytes(plan)?;
    let mut hasher = Sha256::new();
    hasher.update(PLAN_HASH_DOMAIN);
    hasher.update(bytes);
    Ok(PlanHash(hasher.finalize().into()))
}

#[derive(Default)]
struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn bool(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn length(&mut self, value: usize) -> Result<(), CanonicalEncodeError> {
        self.u32(
            value
                .try_into()
                .map_err(|_| CanonicalEncodeError::LengthOverflow)?,
        );
        Ok(())
    }

    fn string(&mut self, value: &str) -> Result<(), CanonicalEncodeError> {
        self.length(value.len())?;
        self.bytes.extend_from_slice(value.as_bytes());
        Ok(())
    }

    fn list<T: CanonicalEncode>(&mut self, values: &[T]) -> Result<(), CanonicalEncodeError> {
        self.length(values.len())?;
        for value in values {
            value.encode(self)?;
        }
        Ok(())
    }

    fn optional<T: CanonicalEncode>(
        &mut self,
        value: &Option<T>,
    ) -> Result<(), CanonicalEncodeError> {
        match value {
            Some(value) => {
                self.u8(1);
                value.encode(self)
            }
            None => {
                self.u8(0);
                Ok(())
            }
        }
    }
}

trait CanonicalEncode {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError>;
}

impl CanonicalEncode for String {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        encoder.string(self)
    }
}

impl<T: CanonicalEncode> CanonicalEncode for Box<T> {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        self.as_ref().encode(encoder)
    }
}

impl CanonicalEncode for Plan {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        encoder.string(&self.schema)?;
        encoder.string(&self.name)?;
        self.flow.encode(encoder)?;
        self.interface.encode(encoder)?;
        encoder.list(&self.contracts)?;
        encoder.list(&self.tasks)?;
        encoder.list(&self.regions)?;
        encoder.list(&self.loops)?;
        self.capabilities.encode(encoder)?;
        self.resolution.encode(encoder)
    }
}

impl CanonicalEncode for FlowIdentity {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        encoder.string(&self.schema)?;
        encoder.string(&self.semantic_sha256)
    }
}

impl CanonicalEncode for PlanInterface {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        encoder.list(&self.inputs)?;
        encoder.list(&self.outputs)
    }
}

impl CanonicalEncode for PlanContract {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        encoder.string(&self.id)?;
        self.expression.encode(encoder)
    }
}

impl CanonicalEncode for PlanPort {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        encoder.string(&self.name)?;
        self.value_type.encode(encoder)?;
        encoder.bool(self.optional);
        Ok(())
    }
}

impl CanonicalEncode for PlanValueType {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        match self {
            Self::Bool => encoder.u8(0),
            Self::Int => encoder.u8(1),
            Self::String => encoder.u8(2),
            Self::Path => encoder.u8(3),
            Self::Evidence => encoder.u8(4),
            Self::ContractReport => encoder.u8(5),
            Self::Receipt => encoder.u8(6),
            Self::Artifact { media_type, schema } => {
                encoder.u8(7);
                encoder.optional(media_type)?;
                encoder.optional(schema)?;
            }
            Self::List { item } => {
                encoder.u8(8);
                item.encode(encoder)?;
            }
        }
        Ok(())
    }
}

impl CanonicalEncode for PlanContractExpression {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        match self {
            Self::Predicate {
                name,
                arguments,
                resolution,
            } => {
                encoder.u8(0);
                encoder.string(name)?;
                encode_json_map(arguments, encoder)?;
                encoder.optional(resolution)?;
            }
            Self::All { clauses } => {
                encoder.u8(1);
                encoder.list(clauses)?;
            }
            Self::Any { clauses } => {
                encoder.u8(2);
                encoder.list(clauses)?;
            }
            Self::Not { clause } => {
                encoder.u8(3);
                clause.encode(encoder)?;
            }
        }
        Ok(())
    }
}

impl CanonicalEncode for TaskTemplate {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        encoder.string(&self.id)?;
        self.operation.encode(encoder)?;
        encoder.list(&self.inputs)?;
        encoder.list(&self.outputs)?;
        self.readiness.encode(encoder)?;
        encoder.list(&self.region_path)?;
        encoder.optional(&self.transaction)?;
        encoder.optional(&self.loop_context)?;
        self.capabilities.encode(encoder)?;
        self.resources.encode(encoder)
    }
}

impl CanonicalEncode for TaskOperation {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        match self {
            Self::Input => encoder.u8(0),
            Self::Output => encoder.u8(1),
            Self::Program {
                source,
                entrypoint,
                resolution,
            } => {
                encoder.u8(2);
                encoder.string(source)?;
                encoder.optional(entrypoint)?;
                encoder.optional(resolution)?;
            }
            Self::Gate { contract } => {
                encoder.u8(3);
                encoder.string(contract)?;
            }
            Self::Join { strategy } => {
                encoder.u8(4);
                strategy.encode(encoder)?;
            }
            Self::Promote { paths } => {
                encoder.u8(5);
                encoder.list(paths)?;
            }
            Self::Discard => encoder.u8(6),
        }
        Ok(())
    }
}

impl CanonicalEncode for InputBinding {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        self.port.encode(encoder)?;
        encoder.list(&self.sources)
    }
}

impl CanonicalEncode for ValueBinding {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        self.from.encode(encoder)?;
        self.selection.encode(encoder)
    }
}

impl CanonicalEncode for PortReference {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        encoder.string(&self.task)?;
        encoder.string(&self.port)
    }
}

impl CanonicalEncode for ValueSelection {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        match self {
            Self::Direct => encoder.u8(0),
            Self::LoopInitial { loop_region } => {
                encoder.u8(1);
                encoder.string(loop_region)?;
            }
            Self::CurrentIteration { loop_region } => {
                encoder.u8(2);
                encoder.string(loop_region)?;
            }
            Self::LoopFinal { loop_region } => {
                encoder.u8(3);
                encoder.string(loop_region)?;
            }
            Self::PreviousIteration { loop_region, state } => {
                encoder.u8(4);
                encoder.string(loop_region)?;
                encoder.string(state)?;
            }
        }
        Ok(())
    }
}

impl CanonicalEncode for Readiness {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        self.data.encode(encoder)?;
        encoder.list(&self.after)?;
        encoder.list(&self.activate_on_any)
    }
}

impl CanonicalEncode for DataReadiness {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        self.mode.encode(encoder)?;
        encoder.list(&self.tasks)
    }
}

impl CanonicalEncode for ReadinessMode {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        encoder.u8(match self {
            Self::All => 0,
            Self::Any => 1,
        });
        Ok(())
    }
}

impl CanonicalEncode for RouteCondition {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        encoder.string(&self.gate)?;
        self.outcome.encode(encoder)
    }
}

impl CanonicalEncode for RegionTemplate {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        encoder.string(&self.id)?;
        encoder.optional(&self.parent)?;
        self.kind.encode(encoder)?;
        encoder.list(&self.direct_members)
    }
}

impl CanonicalEncode for PlanRegionKind {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        match self {
            Self::Scope => encoder.u8(0),
            Self::Parallel => encoder.u8(1),
            Self::Transaction { workspace } => {
                encoder.u8(2);
                encoder.string(workspace)?;
            }
            Self::Loop => encoder.u8(3),
        }
        Ok(())
    }
}

impl CanonicalEncode for LoopTemplate {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        encoder.string(&self.id)?;
        encoder.list(&self.body_order)?;
        encoder.u32(self.max_iterations);
        self.until.encode(encoder)?;
        self.on_exhausted.encode(encoder)?;
        encoder.list(&self.states)
    }
}

impl CanonicalEncode for LoopState {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        encoder.string(&self.name)?;
        self.value_type.encode(encoder)?;
        self.target.encode(encoder)?;
        self.initial.encode(encoder)?;
        self.feedback.encode(encoder)
    }
}

macro_rules! encode_unit_enum {
    ($type:ty, $($variant:path => $tag:expr),+ $(,)?) => {
        impl CanonicalEncode for $type {
            fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
                encoder.u8(match self { $($variant => $tag),+ });
                Ok(())
            }
        }
    };
}

encode_unit_enum!(
    PlanJoinStrategy,
    PlanJoinStrategy::All => 0,
    PlanJoinStrategy::Any => 1,
    PlanJoinStrategy::FirstValid => 2,
);
encode_unit_enum!(
    PlanGateOutcome,
    PlanGateOutcome::Valid => 0,
    PlanGateOutcome::Invalid => 1,
    PlanGateOutcome::Error => 2,
    PlanGateOutcome::Cancelled => 3,
);
encode_unit_enum!(
    PlanExhaustionPolicy,
    PlanExhaustionPolicy::Fail => 0,
    PlanExhaustionPolicy::YieldLast => 1,
    PlanExhaustionPolicy::Discard => 2,
);

impl CanonicalEncode for PlanResources {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        encode_optional_u64(self.timeout_ms, encoder);
        encode_optional_u64(self.memory_bytes, encoder);
        encode_optional_u64(self.output_bytes, encoder);
        match self.attempts {
            Some(value) => {
                encoder.u8(1);
                encoder.u32(value);
            }
            None => encoder.u8(0),
        }
        Ok(())
    }
}

fn encode_optional_u64(value: Option<u64>, encoder: &mut Encoder) {
    match value {
        Some(value) => {
            encoder.u8(1);
            encoder.u64(value);
        }
        None => encoder.u8(0),
    }
}

impl CanonicalEncode for CapabilitySummary {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        encoder.list(&self.read)?;
        encoder.list(&self.write)?;
        encoder.list(&self.execute)?;
        encoder.list(&self.network)
    }
}

impl CanonicalEncode for ResolvedProgram {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        encoder.string(&self.manifest_sha256)?;
        encoder.string(&self.source_sha256)?;
        encoder.string(&self.entrypoint)
    }
}

impl CanonicalEncode for ResolvedPredicate {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        encoder.string(&self.definition_sha256)?;
        encoder.string(&self.version)?;
        encoder.string(&self.implementation_sha256)
    }
}

impl CanonicalEncode for ResolutionStatus {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        encoder.bool(self.fully_resolved);
        encoder.list(&self.blockers)
    }
}

impl CanonicalEncode for ResolutionBlocker {
    fn encode(&self, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
        match self {
            Self::UnresolvedProgram { task, source } => {
                encoder.u8(0);
                encoder.string(task)?;
                encoder.string(source)?;
            }
            Self::UncheckedPredicate {
                contract,
                predicate,
            } => {
                encoder.u8(1);
                encoder.string(contract)?;
                encoder.string(predicate)?;
            }
        }
        Ok(())
    }
}

fn encode_json_map(
    values: &BTreeMap<String, Value>,
    encoder: &mut Encoder,
) -> Result<(), CanonicalEncodeError> {
    encoder.length(values.len())?;
    let mut entries: Vec<_> = values.iter().collect();
    entries.sort_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));
    for (key, value) in entries {
        encoder.string(key)?;
        encode_json(value, encoder)?;
    }
    Ok(())
}

fn encode_json(value: &Value, encoder: &mut Encoder) -> Result<(), CanonicalEncodeError> {
    match value {
        Value::Null => encoder.u8(0),
        Value::Bool(false) => encoder.u8(1),
        Value::Bool(true) => encoder.u8(2),
        Value::Number(number) => {
            if let Some(value) = number.as_u64() {
                encoder.u8(3);
                encoder.u64(value);
            } else if let Some(value) = number.as_i64() {
                encoder.u8(4);
                encoder.i64(value);
            } else if let Some(mut value) = number.as_f64() {
                encoder.u8(5);
                if value == 0.0 {
                    value = 0.0;
                }
                encoder.u64(value.to_bits());
            } else {
                return Err(CanonicalEncodeError::UnsupportedNumber);
            }
        }
        Value::String(value) => {
            encoder.u8(6);
            encoder.string(value)?;
        }
        Value::Array(values) => {
            encoder.u8(7);
            encoder.length(values.len())?;
            for value in values {
                encode_json(value, encoder)?;
            }
        }
        Value::Object(values) => {
            encoder.u8(8);
            encoder.length(values.len())?;
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));
            for (key, value) in entries {
                encoder.string(key)?;
                encode_json(value, encoder)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives_have_fixed_big_endian_encodings() {
        let mut encoder = Encoder::default();
        encoder.u32(0x0102_0304);
        encoder.u64(0x0102_0304_0506_0708);
        encoder.i64(-2);
        assert_eq!(
            encoder.bytes,
            [
                1, 2, 3, 4, 1, 2, 3, 4, 5, 6, 7, 8, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe,
            ]
        );
    }

    #[test]
    fn strings_are_length_prefixed_utf8_without_normalization() {
        let mut composed = Encoder::default();
        composed.string("é").unwrap();
        assert_eq!(composed.bytes, [0, 0, 0, 2, 0xc3, 0xa9]);

        let mut decomposed = Encoder::default();
        decomposed.string("e\u{301}").unwrap();
        assert_eq!(decomposed.bytes, [0, 0, 0, 3, 0x65, 0xcc, 0x81]);
        assert_ne!(composed.bytes, decomposed.bytes);
    }

    #[test]
    fn json_objects_sort_keys_by_utf8_bytes() {
        let value = serde_json::json!({"z": 1, "a": 2});
        let mut encoder = Encoder::default();
        encode_json(&value, &mut encoder).unwrap();
        assert!(
            encoder
                .bytes
                .windows(5)
                .any(|bytes| bytes == [0, 0, 0, 1, b'a'])
        );
        let a = encoder
            .bytes
            .windows(5)
            .position(|bytes| bytes == [0, 0, 0, 1, b'a'])
            .unwrap();
        let z = encoder
            .bytes
            .windows(5)
            .position(|bytes| bytes == [0, 0, 0, 1, b'z'])
            .unwrap();
        assert!(a < z);
    }

    #[test]
    fn negative_zero_has_one_canonical_float_encoding() {
        let mut positive = Encoder::default();
        encode_json(&serde_json::json!(0.0), &mut positive).unwrap();
        let mut negative = Encoder::default();
        encode_json(&serde_json::json!(-0.0), &mut negative).unwrap();
        assert_eq!(positive.bytes, negative.bytes);
    }
}

use crate::{Flow, NodeKind};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fmt;
use thiserror::Error;

const HASH_DOMAIN: &[u8] = b"xshell.flow.semantic.v0\0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticHash([u8; 32]);

impl SemanticHash {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for SemanticHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex::encode(self.0))
    }
}

#[derive(Debug, Error)]
pub enum HashError {
    #[error("could not encode semantic flow: {0}")]
    Encode(#[from] serde_json::Error),
}

#[derive(Serialize)]
struct SemanticFlow<'a> {
    schema: &'a str,
    name: &'a str,
    interface: crate::FlowInterface,
    contracts: Vec<crate::Contract>,
    nodes: Vec<SemanticNode<'a>>,
    edges: Vec<crate::Edge>,
    regions: Vec<SemanticRegion<'a>>,
}

#[derive(Serialize)]
struct SemanticNode<'a> {
    id: &'a str,
    inputs: Vec<crate::Port>,
    outputs: Vec<crate::Port>,
    #[serde(flatten)]
    kind: &'a NodeKind,
    capabilities: crate::Capabilities,
    resources: &'a crate::Resources,
}

#[derive(Serialize)]
struct SemanticRegion<'a> {
    id: &'a str,
    parent: &'a Option<String>,
    nodes: Vec<String>,
    #[serde(flatten)]
    kind: &'a crate::RegionKind,
}

pub fn canonical_semantic_bytes(flow: &Flow) -> Result<Vec<u8>, HashError> {
    let mut interface = flow.interface.clone();
    interface
        .inputs
        .sort_by(|left, right| left.name.cmp(&right.name));
    interface
        .outputs
        .sort_by(|left, right| left.name.cmp(&right.name));

    let mut contracts = flow.contracts.clone();
    contracts.sort_by(|left, right| left.id.cmp(&right.id));

    let mut nodes: Vec<_> = flow
        .nodes
        .iter()
        .map(|node| {
            let mut inputs = node.inputs.clone();
            inputs.sort_by(|left, right| left.name.cmp(&right.name));
            let mut outputs = node.outputs.clone();
            outputs.sort_by(|left, right| left.name.cmp(&right.name));
            let mut capabilities = node.capabilities.clone();
            capabilities.read.sort();
            capabilities.write.sort();
            capabilities.execute.sort();
            capabilities.network.sort();
            SemanticNode {
                id: &node.id,
                inputs,
                outputs,
                kind: &node.kind,
                capabilities,
                resources: &node.resources,
            }
        })
        .collect();
    nodes.sort_by(|left, right| left.id.cmp(right.id));

    let mut edges = flow.edges.clone();
    edges.sort_by(|left, right| left.id.cmp(&right.id));

    let mut regions: Vec<_> = flow
        .regions
        .iter()
        .map(|region| {
            let mut nodes = region.nodes.clone();
            nodes.sort();
            SemanticRegion {
                id: &region.id,
                parent: &region.parent,
                nodes,
                kind: &region.kind,
            }
        })
        .collect();
    regions.sort_by(|left, right| left.id.cmp(right.id));

    Ok(serde_json::to_vec(&SemanticFlow {
        schema: &flow.schema,
        name: &flow.name,
        interface,
        contracts,
        nodes,
        edges,
        regions,
    })?)
}

pub fn semantic_hash(flow: &Flow) -> Result<SemanticHash, HashError> {
    let encoded = canonical_semantic_bytes(flow)?;
    let mut hasher = Sha256::new();
    hasher.update(HASH_DOMAIN);
    hasher.update(encoded);
    Ok(SemanticHash(hasher.finalize().into()))
}

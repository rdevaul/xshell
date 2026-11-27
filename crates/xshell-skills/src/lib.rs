//! Verification of signed skills (docs/signed-skills.md §3 and §4).
//!
//! A skill is a directory with `MANIFEST.json` and `MANIFEST.json.sig` at
//! its top: the manifest lists every file, and the signature is an OpenSSH
//! signature (`ssh-keygen -Y sign -n skill-manifest`) over the manifest's
//! exact bytes. [`verify_skill`] accepts a directory only if the signature
//! is by a key the operator trusts and has not revoked, the manifest is well
//! formed, and the directory holds exactly the listed files.
//!
//! Signatures are checked in this process, not by running `ssh-keygen`, so
//! the result does not depend on what is installed on the host. Only
//! Ed25519 keys are supported.
//!
//! This crate only verifies. It does not install, load or run anything.

mod directory;
mod error;
mod manifest;
mod signers;
mod sshsig;

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

pub use error::SkillError;
pub use manifest::{
    FileEntry, MANIFEST_NAME, MANIFEST_VERSION, MAX_DESCRIPTION_CHARS, MAX_FILES,
    MAX_MANIFEST_BYTES, MAX_PATH_BYTES, MAX_SIGNATURE_BYTES, MAX_TOTAL_BYTES, Manifest, Requires,
    SIGNATURE_NAME, SkillNetwork,
};
pub use signers::{RevokedKeys, TrustedSigners};
pub use sshsig::PublicKey;

/// The OpenSSH signature namespace for skill manifests.
pub const NAMESPACE: &str = "skill-manifest";

/// What the operator trusts, and when the question is being asked.
#[derive(Clone, Copy, Debug)]
pub struct Trust<'a> {
    pub signers: &'a TrustedSigners,
    pub revoked: &'a RevokedKeys,
    /// Checked against a signer's `valid-after` and `valid-before`.
    pub now: SystemTime,
}

/// A manifest whose signature was verified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedManifest {
    pub manifest: Manifest,
    /// The SHA-256 of `MANIFEST.json`'s bytes, in lowercase hex. This is the
    /// skill's identity: it names every file and every byte.
    pub manifest_sha256: String,
    /// The principal, from the trusted-signers file, whose key signed it.
    pub signer: String,
    /// The key that signed it.
    pub key: PublicKey,
}

/// Check a manifest's signature and then parse it.
///
/// The bytes are not parsed until the signature over them has been verified
/// by a trusted, unrevoked key.
pub fn verify_manifest(
    manifest: &[u8],
    signature: &[u8],
    trust: Trust<'_>,
) -> Result<VerifiedManifest, SkillError> {
    if manifest.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(SkillError::Manifest(format!(
            "{MANIFEST_NAME} is larger than 1 MiB"
        )));
    }
    if signature.len() as u64 > MAX_SIGNATURE_BYTES {
        return Err(SkillError::Signature(
            "the signature file is too large".into(),
        ));
    }
    let signature = sshsig::SshSignature::parse(signature)?;
    signature.verify(manifest, NAMESPACE)?;
    if trust.revoked.contains(&signature.key) {
        return Err(SkillError::RevokedKey {
            fingerprint: signature.key.fingerprint(),
        });
    }
    let now = trust
        .now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let signer = trust
        .signers
        .signer_for(&signature.key, NAMESPACE, now)?
        .to_owned();
    Ok(VerifiedManifest {
        manifest: Manifest::parse(manifest)?,
        manifest_sha256: hex::encode(Sha256::digest(manifest)),
        signer,
        key: signature.key,
    })
}

/// Check a skill directory: signature, manifest, and every file.
pub fn verify_skill(dir: &Path, trust: Trust<'_>) -> Result<VerifiedManifest, SkillError> {
    let manifest = directory::read_top_file(dir, MANIFEST_NAME, MAX_MANIFEST_BYTES)?;
    let signature = directory::read_top_file(dir, SIGNATURE_NAME, MAX_SIGNATURE_BYTES)?;
    let verified = verify_manifest(&manifest, &signature, trust)?;
    directory::check(dir, &verified.manifest)?;
    Ok(verified)
}

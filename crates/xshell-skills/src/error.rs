/// Why a skill, a signature or a trust file was refused.
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    /// A file could not be read.
    #[error("{0}")]
    Io(String),
    /// The signature file is malformed, or does not verify.
    #[error("signature: {0}")]
    Signature(String),
    /// A key type, certificate or option this verifier does not support.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The signing key is not in the trusted-signers file.
    #[error("the signing key {fingerprint} does not belong to any trusted signer")]
    UntrustedKey { fingerprint: String },
    /// The signing key is in the revoked-keys file.
    #[error("the signing key {fingerprint} is revoked")]
    RevokedKey { fingerprint: String },
    /// The key belongs to a trusted signer, but not for this namespace or time.
    #[error("signer {principal}: {reason}")]
    SignerNotValid { principal: String, reason: String },
    /// `MANIFEST.json` breaks a rule.
    #[error("manifest: {0}")]
    Manifest(String),
    /// The directory does not match the manifest.
    #[error("skill directory: {0}")]
    Directory(String),
    /// The trusted-signers or revoked-keys file is malformed.
    #[error("{file} line {line}: {reason}")]
    TrustFile {
        file: &'static str,
        line: usize,
        reason: String,
    },
}

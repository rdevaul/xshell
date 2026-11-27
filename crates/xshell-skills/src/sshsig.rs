//! OpenSSH signatures (`ssh-keygen -Y sign`), Ed25519 only.
//!
//! The format is OpenSSH's PROTOCOL.sshsig. The armored file holds
//!
//! ```text
//! byte[6]  "SSHSIG"
//! uint32   version (1)
//! string   public key
//! string   namespace
//! string   reserved
//! string   hash algorithm ("sha256" or "sha512")
//! string   signature
//! ```
//!
//! and the signature is over
//!
//! ```text
//! byte[6]  "SSHSIG"
//! string   namespace
//! string   reserved
//! string   hash algorithm
//! string   H(message)
//! ```
//!
//! Where this parser and `ssh-keygen` could disagree it is the stricter of
//! the two: trailing bytes, a non-empty reserved field and text around the
//! armor are refused.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256, Sha512};

use crate::SkillError;

const BEGIN: &str = "-----BEGIN SSH SIGNATURE-----";
const END: &str = "-----END SSH SIGNATURE-----";
const MAGIC: &[u8] = b"SSHSIG";
const ED25519: &str = "ssh-ed25519";

/// An Ed25519 public key, as OpenSSH identifies it.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicKey([u8; 32]);

impl PublicKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The key in OpenSSH's wire encoding: `string "ssh-ed25519", string key`.
    pub(crate) fn blob(&self) -> Vec<u8> {
        let mut blob = Vec::with_capacity(51);
        put_string(&mut blob, ED25519.as_bytes());
        put_string(&mut blob, &self.0);
        blob
    }

    /// `SHA256:…`, as `ssh-keygen -l` prints it.
    pub fn fingerprint(&self) -> String {
        fingerprint_of_blob(&self.blob())
    }

    /// Parse a key's wire encoding. Any type but Ed25519 is unsupported.
    pub(crate) fn from_blob(blob: &[u8]) -> Result<Self, SkillError> {
        let mut reader = Reader(blob);
        let kind = reader.text("key type")?;
        if kind != ED25519 {
            return Err(SkillError::Unsupported(format!(
                "key type {kind}; only {ED25519} keys can sign skills"
            )));
        }
        let key = reader.string("key")?;
        reader.finish("public key")?;
        let key: [u8; 32] = key
            .try_into()
            .map_err(|_| SkillError::Signature("an Ed25519 key must be 32 bytes".into()))?;
        Ok(Self(key))
    }
}

impl std::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.fingerprint())
    }
}

/// `SHA256:` and the unpadded base64 SHA-256 of a key's wire encoding.
pub(crate) fn fingerprint_of_blob(blob: &[u8]) -> String {
    format!("SHA256:{}", STANDARD_NO_PAD.encode(Sha256::digest(blob)))
}

pub(crate) fn decode_base64(text: &str) -> Option<Vec<u8>> {
    STANDARD.decode(text).ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HashAlgorithm {
    Sha256,
    Sha512,
}

impl HashAlgorithm {
    fn name(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
        }
    }

    fn digest(self, message: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha256 => Sha256::digest(message).to_vec(),
            Self::Sha512 => Sha512::digest(message).to_vec(),
        }
    }
}

/// A parsed signature file. Nothing has been verified yet.
#[derive(Debug)]
pub(crate) struct SshSignature {
    pub(crate) key: PublicKey,
    pub(crate) namespace: String,
    hash: HashAlgorithm,
    signature: [u8; 64],
}

impl SshSignature {
    pub(crate) fn parse(armored: &[u8]) -> Result<Self, SkillError> {
        let blob = dearmor(armored)?;
        let rest = blob
            .strip_prefix(MAGIC)
            .ok_or_else(|| malformed("it does not start with SSHSIG"))?;
        let mut reader = Reader(rest);
        let version = reader.u32("version")?;
        if version != 1 {
            return Err(SkillError::Unsupported(format!(
                "signature format version {version}"
            )));
        }
        let key = PublicKey::from_blob(reader.string("public key")?)?;
        let namespace = reader.text("namespace")?.to_owned();
        if namespace.is_empty() {
            return Err(malformed("the namespace is empty"));
        }
        if !reader.string("reserved field")?.is_empty() {
            return Err(SkillError::Unsupported(
                "a signature with a non-empty reserved field".into(),
            ));
        }
        let hash = match reader.text("hash algorithm")? {
            "sha256" => HashAlgorithm::Sha256,
            "sha512" => HashAlgorithm::Sha512,
            other => {
                return Err(SkillError::Unsupported(format!("hash algorithm {other}")));
            }
        };
        let mut inner = Reader(reader.string("signature")?);
        reader.finish("signature file")?;
        let kind = inner.text("signature type")?;
        if kind != ED25519 {
            return Err(SkillError::Unsupported(format!("signature type {kind}")));
        }
        let signature: [u8; 64] = inner
            .string("signature bytes")?
            .try_into()
            .map_err(|_| malformed("an Ed25519 signature must be 64 bytes"))?;
        inner.finish("signature")?;
        Ok(Self {
            key,
            namespace,
            hash,
            signature,
        })
    }

    /// Check the signature over `message`, made for `namespace`.
    pub(crate) fn verify(&self, message: &[u8], namespace: &str) -> Result<(), SkillError> {
        if self.namespace != namespace {
            return Err(SkillError::Signature(format!(
                "it was made for namespace {:?}, not {namespace:?}",
                self.namespace
            )));
        }
        let mut signed = Vec::with_capacity(128);
        signed.extend_from_slice(MAGIC);
        put_string(&mut signed, namespace.as_bytes());
        put_string(&mut signed, b"");
        put_string(&mut signed, self.hash.name().as_bytes());
        put_string(&mut signed, &self.hash.digest(message));

        let key = VerifyingKey::from_bytes(self.key.as_bytes())
            .map_err(|_| SkillError::Signature("the public key is not a valid point".into()))?;
        // `verify_strict` also refuses weak keys and non-canonical signatures.
        key.verify_strict(&signed, &Signature::from_bytes(&self.signature))
            .map_err(|_| SkillError::Signature("it does not match the signed file".into()))
    }
}

fn malformed(what: &str) -> SkillError {
    SkillError::Signature(format!("the signature file is malformed: {what}"))
}

fn dearmor(armored: &[u8]) -> Result<Vec<u8>, SkillError> {
    let text = std::str::from_utf8(armored).map_err(|_| malformed("it is not text"))?;
    let mut lines = text.lines().map(str::trim_end);
    if lines.next() != Some(BEGIN) {
        return Err(malformed("the first line is not the BEGIN line"));
    }
    let mut body = String::new();
    let mut ended = false;
    for line in lines {
        if ended {
            if !line.is_empty() {
                return Err(malformed("there is text after the END line"));
            }
        } else if line == END {
            ended = true;
        } else {
            body.push_str(line);
        }
    }
    if !ended {
        return Err(malformed("the END line is missing"));
    }
    decode_base64(&body).ok_or_else(|| malformed("the body is not base64"))
}

pub(crate) fn put_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// A cursor over SSH wire data.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn u32(&mut self, what: &str) -> Result<u32, SkillError> {
        let (head, rest) = self
            .0
            .split_first_chunk::<4>()
            .ok_or_else(|| malformed(&format!("it ends inside the {what}")))?;
        self.0 = rest;
        Ok(u32::from_be_bytes(*head))
    }

    fn string(&mut self, what: &str) -> Result<&'a [u8], SkillError> {
        let length = self.u32(what)? as usize;
        if length > self.0.len() {
            return Err(malformed(&format!("it ends inside the {what}")));
        }
        let (value, rest) = self.0.split_at(length);
        self.0 = rest;
        Ok(value)
    }

    fn text(&mut self, what: &str) -> Result<&'a str, SkillError> {
        std::str::from_utf8(self.string(what)?)
            .map_err(|_| malformed(&format!("the {what} is not text")))
    }

    fn finish(&self, what: &str) -> Result<(), SkillError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(malformed(&format!(
                "there are extra bytes after the {what}"
            )))
        }
    }
}

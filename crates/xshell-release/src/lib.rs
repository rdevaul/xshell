use anyhow::{Context, Result, bail};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::time::Duration;

pub const RELEASE_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const MAX_RELEASE_MANIFEST_BYTES: usize = 1024 * 1024;
pub const MAX_RELEASE_ARTIFACT_BYTES: u64 = 128 * 1024 * 1024;
const RELEASE_SIGNATURE_DOMAIN: &[u8] = b"xshell-release-manifest-v1\0";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifestBody {
    pub schema_version: u32,
    pub version: String,
    pub protocol_version: u32,
    pub artifacts: Vec<ReleaseArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseArtifact {
    pub target: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SignedReleaseManifest {
    #[serde(flatten)]
    pub release: ReleaseManifestBody,
    pub signing_key_id: String,
    pub signature: String,
}

#[derive(Debug, Clone)]
pub struct VerifiedReleaseManifest(SignedReleaseManifest);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseSource {
    pub manifest_url: String,
    pub public_key: String,
}

pub struct PreparedReleaseArtifact {
    pub manifest_url: String,
    pub version: String,
    pub protocol_version: u32,
    pub signing_key_id: String,
    pub artifact: ReleaseArtifact,
    pub bytes: Vec<u8>,
}

impl ReleaseSource {
    pub fn validate(&self) -> Result<()> {
        let _ = self.manifest_url_for("0.0.0")?;
        let _ = parse_public_key(&self.public_key)?;
        Ok(())
    }

    pub fn manifest_url_for(&self, version: &str) -> Result<String> {
        if version.trim().is_empty()
            || !version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            bail!("release version contains unsupported URL characters");
        }
        let url = self.manifest_url.replace("{version}", version);
        if url.contains('{') || url.contains('}') {
            bail!("release manifest URL contains an unsupported template placeholder");
        }
        require_https_url(&url)?;
        Ok(url)
    }

    pub async fn acquire(
        &self,
        target: &str,
        required_version: &str,
        required_protocol_version: u32,
    ) -> Result<PreparedReleaseArtifact> {
        self.validate()?;
        let manifest_url = self.manifest_url_for(required_version)?;
        let manifest = fetch_verified_manifest(&manifest_url, &self.public_key).await?;
        let artifact = manifest
            .select_artifact(target, required_version, required_protocol_version)?
            .clone();
        let bytes = fetch_verified_artifact(&artifact).await?;
        Ok(PreparedReleaseArtifact {
            manifest_url,
            version: manifest.signed().release.version.clone(),
            protocol_version: manifest.signed().release.protocol_version,
            signing_key_id: manifest.signed().signing_key_id.clone(),
            artifact,
            bytes,
        })
    }
}

impl VerifiedReleaseManifest {
    pub fn signed(&self) -> &SignedReleaseManifest {
        &self.0
    }

    pub fn select_artifact(
        &self,
        target: &str,
        required_version: &str,
        required_protocol_version: u32,
    ) -> Result<&ReleaseArtifact> {
        let body = &self.0.release;
        if body.version != required_version {
            bail!(
                "release manifest describes xshell {}, but this controller requires {}",
                body.version,
                required_version
            );
        }
        if body.protocol_version != required_protocol_version {
            bail!(
                "release manifest describes protocol {}, but this controller requires {}",
                body.protocol_version,
                required_protocol_version
            );
        }
        body.artifacts
            .iter()
            .find(|artifact| artifact.target == target)
            .with_context(|| format!("release has no xshelld artifact for target {target:?}"))
    }
}

pub fn sign_manifest(
    body: ReleaseManifestBody,
    signing_key: &SigningKey,
) -> Result<SignedReleaseManifest> {
    validate_body(&body)?;
    let signature = signing_key.sign(&signature_message(&body)?);
    Ok(SignedReleaseManifest {
        release: body,
        signing_key_id: signing_key_id(&signing_key.verifying_key()),
        signature: hex::encode(signature.to_bytes()),
    })
}

pub fn verify_manifest(source: &[u8], public_key_hex: &str) -> Result<VerifiedReleaseManifest> {
    if source.len() > MAX_RELEASE_MANIFEST_BYTES {
        bail!("release manifest exceeds {MAX_RELEASE_MANIFEST_BYTES} bytes");
    }
    let manifest: SignedReleaseManifest =
        serde_json::from_slice(source).context("invalid release manifest JSON")?;
    validate_body(&manifest.release)?;
    let verifying_key = parse_public_key(public_key_hex)?;
    if manifest.signing_key_id != signing_key_id(&verifying_key) {
        bail!("release manifest signing key ID does not match the configured public key");
    }
    let signature_bytes = hex::decode(&manifest.signature).context("invalid signature encoding")?;
    let signature = Signature::from_slice(&signature_bytes).context("invalid Ed25519 signature")?;
    verifying_key
        .verify(&signature_message(&manifest.release)?, &signature)
        .context("release manifest signature verification failed")?;
    Ok(VerifiedReleaseManifest(manifest))
}

pub fn verify_artifact(artifact: &ReleaseArtifact, bytes: &[u8]) -> Result<()> {
    validate_artifact(artifact)?;
    if bytes.len() as u64 != artifact.size {
        bail!(
            "release artifact size mismatch: manifest says {} bytes, received {}",
            artifact.size,
            bytes.len()
        );
    }
    let actual = hex::encode(Sha256::digest(bytes));
    if actual != artifact.sha256 {
        bail!("release artifact SHA-256 mismatch");
    }
    Ok(())
}

pub async fn fetch_verified_manifest(
    url: &str,
    public_key_hex: &str,
) -> Result<VerifiedReleaseManifest> {
    let _ = parse_public_key(public_key_hex)?;
    let client = https_client()?;
    let source = fetch_bounded(&client, url, MAX_RELEASE_MANIFEST_BYTES as u64).await?;
    verify_manifest(&source, public_key_hex)
}

pub async fn fetch_verified_artifact(artifact: &ReleaseArtifact) -> Result<Vec<u8>> {
    validate_artifact(artifact)?;
    let client = https_client()?;
    let bytes = fetch_bounded(&client, &artifact.url, artifact.size).await?;
    verify_artifact(artifact, &bytes)?;
    Ok(bytes)
}

pub fn parse_signing_key(private_key_hex: &str) -> Result<SigningKey> {
    let bytes = hex::decode(private_key_hex.trim()).context("invalid private key encoding")?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("release signing key must contain 32 bytes"))?;
    Ok(SigningKey::from_bytes(&bytes))
}

pub fn parse_public_key(public_key_hex: &str) -> Result<VerifyingKey> {
    let bytes = hex::decode(public_key_hex.trim()).context("invalid public key encoding")?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("release public key must contain 32 bytes"))?;
    VerifyingKey::from_bytes(&bytes).context("invalid Ed25519 public key")
}

pub fn signing_key_id(key: &VerifyingKey) -> String {
    hex::encode(&Sha256::digest(key.as_bytes())[..16])
}

fn validate_body(body: &ReleaseManifestBody) -> Result<()> {
    if body.schema_version != RELEASE_MANIFEST_SCHEMA_VERSION {
        bail!(
            "release manifest uses schema {}, but this client requires {}",
            body.schema_version,
            RELEASE_MANIFEST_SCHEMA_VERSION
        );
    }
    if body.version.trim().is_empty() {
        bail!("release manifest version is empty");
    }
    if body.protocol_version == 0 {
        bail!("release manifest protocol version must be positive");
    }
    if body.artifacts.is_empty() {
        bail!("release manifest contains no artifacts");
    }
    let mut targets = HashSet::new();
    for artifact in &body.artifacts {
        if artifact.target.trim().is_empty() || !targets.insert(&artifact.target) {
            bail!("release manifest contains an empty or duplicate artifact target");
        }
        validate_artifact(artifact)?;
    }
    Ok(())
}

fn validate_artifact(artifact: &ReleaseArtifact) -> Result<()> {
    if artifact.size == 0 || artifact.size > MAX_RELEASE_ARTIFACT_BYTES {
        bail!(
            "artifact {} has invalid size {}; maximum is {} bytes",
            artifact.target,
            artifact.size,
            MAX_RELEASE_ARTIFACT_BYTES
        );
    }
    if artifact.sha256.len() != 64
        || !artifact
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("artifact {} has an invalid SHA-256", artifact.target);
    }
    require_https_url(&artifact.url)
}

fn require_https_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).context("invalid release URL")?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        bail!("release URL must use HTTPS and include a host");
    }
    Ok(())
}

fn signature_message(body: &ReleaseManifestBody) -> Result<Vec<u8>> {
    let mut message = RELEASE_SIGNATURE_DOMAIN.to_vec();
    message.extend(serde_json::to_vec(body)?);
    Ok(message)
}

fn https_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .context("cannot initialize release download client")
}

async fn fetch_bounded(client: &reqwest::Client, url: &str, maximum: u64) -> Result<Vec<u8>> {
    require_https_url(url)?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("cannot download release resource {url}"))?
        .error_for_status()
        .with_context(|| format!("release resource returned an error: {url}"))?;
    if response
        .content_length()
        .is_some_and(|length| length > maximum)
    {
        bail!("release resource exceeds {maximum} bytes");
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("cannot read release download")?;
        if bytes.len().saturating_add(chunk.len()) as u64 > maximum {
            bail!("release resource exceeds {maximum} bytes");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

    fn body(bytes: &[u8]) -> ReleaseManifestBody {
        ReleaseManifestBody {
            schema_version: RELEASE_MANIFEST_SCHEMA_VERSION,
            version: "0.2.0".into(),
            protocol_version: 11,
            artifacts: vec![ReleaseArtifact {
                target: "aarch64-apple-darwin".into(),
                url: "https://example.test/xshelld-aarch64-apple-darwin".into(),
                size: bytes.len() as u64,
                sha256: hex::encode(Sha256::digest(bytes)),
            }],
        }
    }

    #[test]
    fn signed_manifest_selects_and_verifies_an_artifact() {
        let key = SigningKey::generate(&mut OsRng);
        let artifact_bytes = b"xshelld binary";
        let manifest = sign_manifest(body(artifact_bytes), &key).unwrap();
        let source = serde_json::to_vec_pretty(&manifest).unwrap();
        let verified =
            verify_manifest(&source, &hex::encode(key.verifying_key().as_bytes())).unwrap();
        let artifact = verified
            .select_artifact("aarch64-apple-darwin", "0.2.0", 11)
            .unwrap();
        verify_artifact(artifact, artifact_bytes).unwrap();
    }

    #[test]
    fn signature_and_artifact_tampering_fail_closed() {
        let key = SigningKey::generate(&mut OsRng);
        let other_key = SigningKey::generate(&mut OsRng);
        let manifest = sign_manifest(body(b"expected"), &key).unwrap();
        let source = serde_json::to_vec(&manifest).unwrap();
        assert!(
            verify_manifest(&source, &hex::encode(other_key.verifying_key().as_bytes())).is_err()
        );

        let mut tampered_manifest = manifest.clone();
        tampered_manifest.release.version = "0.2.1".into();
        let tampered_source = serde_json::to_vec(&tampered_manifest).unwrap();
        assert!(
            verify_manifest(
                &tampered_source,
                &hex::encode(key.verifying_key().as_bytes())
            )
            .is_err()
        );

        let verified =
            verify_manifest(&source, &hex::encode(key.verifying_key().as_bytes())).unwrap();
        let artifact = &verified.signed().release.artifacts[0];
        assert!(verify_artifact(artifact, b"tampered").is_err());
    }

    #[test]
    fn rejects_duplicates_insecure_urls_and_wrong_release_identity() {
        let key = SigningKey::generate(&mut OsRng);
        let mut duplicate = body(b"binary");
        duplicate.artifacts.push(duplicate.artifacts[0].clone());
        assert!(sign_manifest(duplicate, &key).is_err());

        let mut insecure = body(b"binary");
        insecure.artifacts[0].url = "http://example.test/xshelld".into();
        assert!(sign_manifest(insecure, &key).is_err());

        let manifest = sign_manifest(body(b"binary"), &key).unwrap();
        let source = serde_json::to_vec(&manifest).unwrap();
        let verified =
            verify_manifest(&source, &hex::encode(key.verifying_key().as_bytes())).unwrap();
        assert!(
            verified
                .select_artifact("aarch64-apple-darwin", "0.3.0", 11)
                .is_err()
        );
        assert!(
            verified
                .select_artifact("x86_64-unknown-linux-musl", "0.2.0", 11)
                .is_err()
        );
    }

    #[test]
    fn release_source_resolves_version_and_validates_its_trust_anchor() {
        let key = SigningKey::generate(&mut OsRng);
        let source = ReleaseSource {
            manifest_url: "https://example.test/releases/v{version}/xshell-release.json".into(),
            public_key: hex::encode(key.verifying_key().as_bytes()),
        };
        source.validate().unwrap();
        assert_eq!(
            source.manifest_url_for("0.2.0").unwrap(),
            "https://example.test/releases/v0.2.0/xshell-release.json"
        );

        let mut invalid = source.clone();
        invalid.manifest_url = "http://example.test/release.json".into();
        assert!(invalid.validate().is_err());
        invalid = source;
        invalid.public_key = "not-a-key".into();
        assert!(invalid.validate().is_err());
    }
}

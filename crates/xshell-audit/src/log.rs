use crate::model::{
    AUDIT_FORMAT_VERSION, AuditCheckpoint, AuditEvent, AuditRecord, AuditRecordBody,
    CheckpointBody, WitnessCommitment,
};
use anyhow::{Context, Result, bail};
use ed25519_dalek::{Signer, SigningKey};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const RECORD_DOMAIN: &[u8] = b"xshell-audit-record-v1\0";
const CHECKPOINT_DOMAIN: &[u8] = b"xshell-audit-checkpoint-v1\0";
const CHECKPOINT_HASH_DOMAIN: &[u8] = b"xshell-audit-checkpoint-hash-v1\0";
const WITNESS_DOMAIN: &[u8] = b"xshell-audit-witness-v1\0";

#[derive(Clone)]
pub struct SigningIdentity {
    signing_key: SigningKey,
    key_id: String,
}

impl SigningIdentity {
    pub fn load_or_create(directory: &Path) -> Result<Self> {
        ensure_secure_directory(directory)?;
        let private_path = directory.join("signing-key");
        let public_path = directory.join("signing-key.pub");
        if private_path.exists() != public_path.exists() {
            bail!("audit signing key pair is incomplete");
        }
        let signing_key = if private_path.exists() {
            validate_key_file(&private_path, true)?;
            let encoded = fs::read_to_string(&private_path)
                .with_context(|| format!("cannot read signing key {}", private_path.display()))?;
            let bytes =
                hex::decode(encoded.trim()).context("invalid audit signing key encoding")?;
            let bytes: [u8; 32] = bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("audit signing key must contain 32 bytes"))?;
            SigningKey::from_bytes(&bytes)
        } else {
            let signing_key = SigningKey::generate(&mut OsRng);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true).mode(0o600);
            let mut file = options
                .open(&private_path)
                .with_context(|| format!("cannot create signing key {}", private_path.display()))?;
            writeln!(file, "{}", hex::encode(signing_key.to_bytes()))?;
            file.sync_all()?;
            signing_key
        };

        let public_bytes = signing_key.verifying_key().to_bytes();
        let key_id = hex::encode(&Sha256::digest(public_bytes)[..16]);
        if public_path.exists() {
            validate_key_file(&public_path, false)?;
            let existing = fs::read_to_string(&public_path)?;
            if existing.trim() != hex::encode(public_bytes) {
                bail!("audit public key does not match the private signing key");
            }
        } else {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true).mode(0o444);
            let mut file = options
                .open(&public_path)
                .with_context(|| format!("cannot create public key {}", public_path.display()))?;
            writeln!(file, "{}", hex::encode(public_bytes))?;
            file.sync_all()?;
        }

        Ok(Self {
            signing_key,
            key_id,
        })
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    pub fn public_key_hex(&self) -> String {
        hex::encode(self.signing_key.verifying_key().to_bytes())
    }
}

pub struct AuditLogWriter {
    format_version: u32,
    session_id: String,
    client_uid: u32,
    sequence: u64,
    chain_head: String,
    checkpoint_interval: u64,
    checkpoint_sequence: u64,
    checkpoint_head: String,
    identity: SigningIdentity,
    log: BufWriter<File>,
    checkpoint_index: File,
    path: PathBuf,
}

impl AuditLogWriter {
    pub fn create(
        directory: &Path,
        identity: SigningIdentity,
        client_uid: u32,
        checkpoint_interval: u64,
    ) -> Result<Self> {
        Self::create_with_format(
            directory,
            identity,
            client_uid,
            checkpoint_interval,
            AUDIT_FORMAT_VERSION,
        )
    }

    pub(crate) fn create_with_format(
        directory: &Path,
        identity: SigningIdentity,
        client_uid: u32,
        checkpoint_interval: u64,
        format_version: u32,
    ) -> Result<Self> {
        ensure_secure_directory(directory)?;
        if checkpoint_interval == 0 {
            bail!("checkpoint interval must be greater than zero");
        }
        let session_id = Uuid::new_v4().to_string();
        let sessions = directory.join("sessions");
        ensure_secure_directory(&sessions)?;
        let path = sessions.join(format!("{session_id}.jsonl"));
        let log = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("cannot create audit log {}", path.display()))?;
        let checkpoint_index = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(directory.join("checkpoints.jsonl"))
            .context("cannot open audit checkpoint index")?;

        Ok(Self {
            format_version,
            session_id,
            client_uid,
            sequence: 0,
            chain_head: "0".repeat(64),
            checkpoint_interval,
            checkpoint_sequence: 0,
            checkpoint_head: "0".repeat(64),
            identity,
            log: BufWriter::new(log),
            checkpoint_index,
            path,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn signing_key_id(&self) -> &str {
        self.identity.key_id()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&mut self, event: AuditEvent) -> Result<AuditRecord> {
        self.sequence += 1;
        let body = AuditRecordBody {
            format_version: self.format_version,
            session_id: self.session_id.clone(),
            sequence: self.sequence,
            daemon_timestamp_unix_ms: unix_timestamp_ms()?,
            client_uid: self.client_uid,
            previous_hash: self.chain_head.clone(),
            event,
        };
        // Serialize the body once, hash those bytes, and store exactly those
        // bytes: the verifier hashes what the line holds (F14).
        let encoded = encode_record_body(&body)?;
        let record_hash = record_hash(encoded.as_bytes());
        let raw = RawValue::from_string(encoded)?;
        self.write_line(&StoredRecord {
            entry: EntryKind::Record,
            body: &raw,
            record_hash: record_hash.clone(),
        })?;
        self.chain_head = record_hash.clone();
        let record = AuditRecord { body, record_hash };

        if self.sequence.is_multiple_of(self.checkpoint_interval) {
            self.write_checkpoint(false)?;
        }
        Ok(record)
    }

    pub fn close(mut self) -> Result<AuditCheckpoint> {
        self.write_checkpoint(true)
    }

    fn write_checkpoint(&mut self, final_checkpoint: bool) -> Result<AuditCheckpoint> {
        self.checkpoint_sequence += 1;
        let body = CheckpointBody {
            format_version: self.format_version,
            session_id: self.session_id.clone(),
            checkpoint_sequence: self.checkpoint_sequence,
            previous_checkpoint_hash: self.checkpoint_head.clone(),
            sequence: self.sequence,
            daemon_timestamp_unix_ms: unix_timestamp_ms()?,
            chain_head: self.chain_head.clone(),
            signing_key_id: self.identity.key_id().to_owned(),
            final_checkpoint,
        };
        let encoded = serde_json::to_string(&body)?;
        let mut nonce = [0_u8; 32];
        OsRng.fill_bytes(&mut nonce);
        let blinding_nonce = hex::encode(nonce);
        let witness = WitnessCommitment {
            scheme: WITNESS_SCHEME.into(),
            commitment: witness_commitment(encoded.as_bytes(), &nonce),
        };
        let signature = hex::encode(
            self.identity
                .signing_key
                .sign(&checkpoint_message(encoded.as_bytes(), &witness.commitment))
                .to_bytes(),
        );
        let checkpoint_hash =
            checkpoint_hash(encoded.as_bytes(), &blinding_nonce, &witness, &signature);
        let raw = RawValue::from_string(encoded)?;
        let mut stored = StoredCheckpoint {
            entry: Some(EntryKind::Checkpoint),
            body: &raw,
            blinding_nonce,
            witness,
            signature,
            checkpoint_hash,
        };
        self.write_line(&stored)?;
        stored.entry = None;
        append_checkpoint_index(&mut self.checkpoint_index, &stored)?;
        self.checkpoint_head = stored.checkpoint_hash.clone();
        Ok(AuditCheckpoint {
            body,
            blinding_nonce: stored.blinding_nonce,
            witness: stored.witness,
            signature: stored.signature,
            checkpoint_hash: stored.checkpoint_hash,
        })
    }

    fn write_line(&mut self, entry: &impl Serialize) -> Result<()> {
        serde_json::to_writer(&mut self.log, entry)?;
        self.log.write_all(b"\n")?;
        self.log.flush()?;
        self.log.get_ref().sync_data()?;
        Ok(())
    }
}

/// The `entry` tag of a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntryKind {
    Record,
    Checkpoint,
}

/// A record line as stored: the body is kept as the exact bytes that were
/// hashed.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredRecord<'a> {
    pub entry: EntryKind,
    #[serde(borrow)]
    pub body: &'a RawValue,
    pub record_hash: String,
}

/// A checkpoint as stored, in a session log (with `entry`) or in
/// `checkpoints.jsonl` (without it). The body is kept as the exact bytes that
/// were signed and hashed.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredCheckpoint<'a> {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntryKind>,
    #[serde(borrow)]
    pub body: &'a RawValue,
    pub blinding_nonce: String,
    pub witness: WitnessCommitment,
    pub signature: String,
    pub checkpoint_hash: String,
}

/// The encoding of a record body that its writer hashed. For formats 1 to 9
/// the verifier reproduces the hash by encoding the parsed body again, so
/// this must reproduce what those writers emitted.
pub(crate) fn encode_record_body(body: &AuditRecordBody) -> Result<String> {
    // Formats 1 and 2 predate `terminal_stream` and never wrote it; from
    // format 3 on, writers encode `None` as `null`.
    if body.format_version <= 2
        && let AuditEvent::LogicalSessionAttached {
            action,
            session_id,
            name,
            host_id,
            host_alias,
            user,
            terminal_stream: None,
        } = &body.event
    {
        return Ok(serde_json::to_string(&LegacyRecordBody {
            format_version: body.format_version,
            session_id: &body.session_id,
            sequence: body.sequence,
            daemon_timestamp_unix_ms: body.daemon_timestamp_unix_ms,
            client_uid: body.client_uid,
            previous_hash: &body.previous_hash,
            event: LegacyLogicalSessionAttached {
                action,
                session_id,
                name,
                host_id,
                host_alias,
                user,
            },
        })?);
    }
    Ok(serde_json::to_string(body)?)
}

/// `AuditRecordBody` as formats 1 and 2 wrote a `logical_session_attached`
/// record. Field order matters: it is the order those writers used.
#[derive(Serialize)]
struct LegacyRecordBody<'a> {
    format_version: u32,
    session_id: &'a str,
    sequence: u64,
    daemon_timestamp_unix_ms: u64,
    client_uid: u32,
    previous_hash: &'a str,
    event: LegacyLogicalSessionAttached<'a>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename = "logical_session_attached")]
struct LegacyLogicalSessionAttached<'a> {
    action: &'a str,
    session_id: &'a str,
    name: &'a str,
    host_id: &'a str,
    host_alias: &'a str,
    user: &'a str,
}

pub(crate) const WITNESS_SCHEME: &str = "sha256-blinded-v1";

pub(crate) fn record_hash(body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(RECORD_DOMAIN);
    hasher.update(body);
    hex::encode(hasher.finalize())
}

pub(crate) fn witness_commitment(body: &[u8], nonce: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(WITNESS_DOMAIN);
    hasher.update(body);
    hasher.update(nonce);
    hex::encode(hasher.finalize())
}

pub(crate) fn checkpoint_message(body: &[u8], commitment: &str) -> Vec<u8> {
    let mut message = Vec::with_capacity(CHECKPOINT_DOMAIN.len() + body.len() + commitment.len());
    message.extend_from_slice(CHECKPOINT_DOMAIN);
    message.extend_from_slice(body);
    message.extend_from_slice(commitment.as_bytes());
    message
}

pub(crate) fn checkpoint_hash(
    body: &[u8],
    blinding_nonce: &str,
    witness: &WitnessCommitment,
    signature: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(CHECKPOINT_HASH_DOMAIN);
    hasher.update(body);
    hasher.update(blinding_nonce.as_bytes());
    hasher.update(witness.scheme.as_bytes());
    hasher.update(witness.commitment.as_bytes());
    hasher.update(signature.as_bytes());
    hex::encode(hasher.finalize())
}

fn unix_timestamp_ms() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis()
        .try_into()
        .context("system timestamp exceeds audit format")
}

pub fn ensure_secure_directory(path: &Path) -> Result<()> {
    xshell_platform::ensure_secure_directory(path, "audit")
}

fn validate_key_file(path: &Path, private: bool) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect audit key {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("audit key {} must be a regular file", path.display());
    }
    // SAFETY: geteuid(2) takes no arguments and cannot fail.
    if metadata.uid() != unsafe { libc::geteuid() } {
        bail!(
            "audit key {} is not owned by the daemon user",
            path.display()
        );
    }
    if private && metadata.mode() & 0o077 != 0 {
        bail!(
            "private audit key {} has unsafe permissions",
            path.display()
        );
    }
    Ok(())
}

fn append_checkpoint_index(file: &mut File, checkpoint: &StoredCheckpoint<'_>) -> Result<()> {
    let mut encoded = serde_json::to_vec(checkpoint)?;
    encoded.push(b'\n');
    let descriptor = file.as_raw_fd();
    // SAFETY: flock(2) takes only integers; `descriptor` belongs to `file`, which is open until
    // the unlock below.
    if unsafe { libc::flock(descriptor, libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot lock checkpoint index");
    }
    let result = (|| {
        file.write_all(&encoded)?;
        file.sync_data()?;
        Ok(())
    })();
    // SAFETY: flock(2) takes only integers; `file` is still open.
    let unlock_result = unsafe { libc::flock(descriptor, libc::LOCK_UN) };
    if unlock_result != 0 && result.is_ok() {
        return Err(std::io::Error::last_os_error()).context("cannot unlock checkpoint index");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MIN_SUPPORTED_AUDIT_FORMAT_VERSION;
    use crate::verify::verify_log;
    use tempfile::TempDir;

    fn event(text: &str) -> AuditEvent {
        AuditEvent::Input {
            route: "agent".into(),
            text: text.into(),
            images: Vec::new(),
        }
    }

    #[test]
    fn verifies_a_complete_signed_chain_and_detects_edits() {
        let temp = TempDir::new().unwrap();
        let identity = SigningIdentity::load_or_create(temp.path()).unwrap();
        let public_key = temp.path().join("signing-key.pub");
        let mut writer = AuditLogWriter::create(temp.path(), identity, 501, 2).unwrap();
        let path = writer.path().to_owned();
        writer.append(event("first")).unwrap();
        writer.append(event("second")).unwrap();
        writer.close().unwrap();

        let report = verify_log(&path, &public_key).unwrap();
        assert_eq!(report.records, 2);
        assert_eq!(report.checkpoints, 2);
        assert!(report.final_checkpoint);

        let contents = fs::read_to_string(&path).unwrap();
        fs::write(&path, contents.replacen("first", "altered", 1)).unwrap();
        assert!(verify_log(&path, &public_key).is_err());
    }

    #[test]
    fn verifier_remains_backward_compatible_with_every_supported_format() {
        for format in MIN_SUPPORTED_AUDIT_FORMAT_VERSION..AUDIT_FORMAT_VERSION {
            let temp = TempDir::new().unwrap();
            let identity = SigningIdentity::load_or_create(temp.path()).unwrap();
            let public_key = temp.path().join("signing-key.pub");
            let mut writer =
                AuditLogWriter::create_with_format(temp.path(), identity, 501, 2, format).unwrap();
            let path = writer.path().to_owned();
            writer.append(event("pre-upgrade record")).unwrap();
            writer.close().unwrap();

            let report = verify_log(&path, &public_key)
                .unwrap_or_else(|error| panic!("format {format} log failed to verify: {error:#}"));
            assert_eq!(report.records, 1, "format {format}");
            assert!(report.final_checkpoint, "format {format}");
        }
    }

    #[test]
    fn agent_responses_written_before_reasoning_existed_hash_unchanged() {
        // Byte-for-byte the shape format-3 writers produced. The verifier
        // re-serializes each body to check its hash, so the optional fields
        // added in format 4 must not appear when absent.
        let old = r#"{"format_version":3,"session_id":"s","sequence":7,"daemon_timestamp_unix_ms":1,"client_uid":501,"previous_hash":"00","event":{"type":"agent_response","content":"Let me check.","tool_call_count":0,"partial":false}}"#;
        let body: AuditRecordBody = serde_json::from_str(old).unwrap();
        assert_eq!(serde_json::to_string(&body).unwrap(), old);

        let new = old.replace(
            r#""partial":false}"#,
            r#""partial":false,"reasoning":"the tests","finish_reason":"stop"}"#,
        );
        let body: AuditRecordBody = serde_json::from_str(&new).unwrap();
        assert_eq!(serde_json::to_string(&body).unwrap(), new);
    }

    #[test]
    fn records_written_before_images_existed_hash_unchanged() {
        // Byte-for-byte the shapes format-5 writers produced. `images` must
        // not appear when empty, or the verifier's re-serialization would
        // change the hash of every earlier input and tool-result record.
        for old in [
            r#"{"format_version":5,"session_id":"s","sequence":1,"daemon_timestamp_unix_ms":1,"client_uid":501,"previous_hash":"00","event":{"type":"input","route":"agent","text":"hi"}}"#,
            r#"{"format_version":5,"session_id":"s","sequence":2,"daemon_timestamp_unix_ms":1,"client_uid":501,"previous_hash":"00","event":{"type":"tool_result","call_id":"c","name":"read_file","result":"ok"}}"#,
        ] {
            let body: AuditRecordBody = serde_json::from_str(old).unwrap();
            assert_eq!(serde_json::to_string(&body).unwrap(), old);
        }
    }

    #[test]
    fn decisions_written_before_their_basis_existed_hash_unchanged() {
        let old = r#"{"format_version":6,"session_id":"s","sequence":3,"daemon_timestamp_unix_ms":1,"client_uid":501,"previous_hash":"00","event":{"type":"tool_decision","call_id":"c","decision":"approve"}}"#;
        let body: AuditRecordBody = serde_json::from_str(old).unwrap();
        assert_eq!(serde_json::to_string(&body).unwrap(), old);
    }

    #[test]
    fn turn_authority_and_decision_basis_are_signed_with_the_record() {
        let temp = TempDir::new().unwrap();
        let identity = SigningIdentity::load_or_create(temp.path()).unwrap();
        let public_key = temp.path().join("signing-key.pub");
        let mut writer = AuditLogWriter::create(temp.path(), identity, 501, 2).unwrap();
        let path = writer.path().to_owned();
        writer
            .append(AuditEvent::TurnStarted {
                turn_id: "t1".into(),
                approval: "ask".into(),
                requested_approval: Some("auto".into()),
                limits: serde_json::json!({"tool_calls": 256}),
                requested_limits: None,
                base_policy: Some(Box::new(crate::AuditPolicy {
                    sha256: "ab".repeat(32),
                    policy: serde_json::json!({"denied": ["run_shell: rm *"]}),
                })),
                overlay_policy: None,
                provided_tools: Some(Box::new(crate::AuditProvidedTools {
                    sha256: "cd".repeat(32),
                    tools: vec![crate::AuditProvidedTool {
                        provider: "memory".into(),
                        name: "memory_purge".into(),
                        access: "read_write".into(),
                        approval: "required".into(),
                    }],
                })),
                offered_tools: Some(vec!["read_file".into(), "memory_purge".into()]),
            })
            .unwrap();
        writer
            .append(AuditEvent::ToolDecision {
                call_id: "c1".into(),
                decision: "deny".into(),
                basis: Some(crate::AuditDecisionBasis {
                    kind: "entry".into(),
                    policy: Some("base".into()),
                    list: Some("denied".into()),
                    entry: Some("run_shell: rm *".into()),
                }),
                approval: None,
            })
            .unwrap();
        writer.close().unwrap();
        verify_log(&path, &public_key).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        for (original, forged) in [
            (r#""list":"denied""#, r#""list":"allowed""#),
            (r#""approval":"ask""#, r#""approval":"off""#),
            (r#""approval":"required""#, r#""approval":"not_required""#),
            (
                r#""offered_tools":["read_file","#,
                r#""offered_tools":["run_shell","#,
            ),
        ] {
            assert!(contents.contains(original), "{original}");
            fs::write(&path, contents.replacen(original, forged, 1)).unwrap();
            assert!(verify_log(&path, &public_key).is_err(), "{forged} verified");
        }
    }

    #[test]
    fn image_metadata_is_signed_with_the_record() {
        let temp = TempDir::new().unwrap();
        let identity = SigningIdentity::load_or_create(temp.path()).unwrap();
        let public_key = temp.path().join("signing-key.pub");
        let mut writer = AuditLogWriter::create(temp.path(), identity, 501, 2).unwrap();
        let path = writer.path().to_owned();
        let image = crate::AuditImage {
            sha256: "ab".repeat(32),
            media_type: "image/png".into(),
            width: 1568,
            height: 1176,
            byte_len: 204_800,
            label: "renders/bracket.png".into(),
        };
        writer
            .append(AuditEvent::ToolResult {
                call_id: "call-1".into(),
                name: "view_image".into(),
                result: "image attached".into(),
                images: vec![image],
            })
            .unwrap();
        writer.close().unwrap();
        verify_log(&path, &public_key).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains(r#""width":1568"#));
        fs::write(
            &path,
            contents.replacen(r#""width":1568"#, r#""width":1569"#, 1),
        )
        .unwrap();
        assert!(verify_log(&path, &public_key).is_err());
    }

    #[test]
    fn verifier_rejects_a_future_format() {
        let temp = TempDir::new().unwrap();
        let identity = SigningIdentity::load_or_create(temp.path()).unwrap();
        let public_key = temp.path().join("signing-key.pub");
        let mut writer = AuditLogWriter::create_with_format(
            temp.path(),
            identity,
            501,
            2,
            AUDIT_FORMAT_VERSION + 1,
        )
        .unwrap();
        let path = writer.path().to_owned();
        writer.append(event("from the future")).unwrap();
        writer.close().unwrap();

        assert!(verify_log(&path, &public_key).is_err());
    }

    #[test]
    fn reports_a_missing_final_checkpoint() {
        let temp = TempDir::new().unwrap();
        let identity = SigningIdentity::load_or_create(temp.path()).unwrap();
        let public_key = temp.path().join("signing-key.pub");
        let mut writer = AuditLogWriter::create(temp.path(), identity, 501, 1).unwrap();
        let path = writer.path().to_owned();
        writer.append(event("only record")).unwrap();
        drop(writer);

        let report = verify_log(&path, &public_key).unwrap();
        assert!(!report.final_checkpoint);
    }

    #[test]
    fn verifies_an_empty_cleanly_closed_session() {
        let temp = TempDir::new().unwrap();
        let identity = SigningIdentity::load_or_create(temp.path()).unwrap();
        let public_key = temp.path().join("signing-key.pub");
        let writer = AuditLogWriter::create(temp.path(), identity, 501, 2).unwrap();
        let path = writer.path().to_owned();
        writer.close().unwrap();

        let report = verify_log(&path, &public_key).unwrap();
        assert_eq!(report.records, 0);
        assert!(report.final_checkpoint);
    }

    #[test]
    fn detects_a_duplicated_signed_checkpoint() {
        let temp = TempDir::new().unwrap();
        let identity = SigningIdentity::load_or_create(temp.path()).unwrap();
        let public_key = temp.path().join("signing-key.pub");
        let mut writer = AuditLogWriter::create(temp.path(), identity, 501, 1).unwrap();
        let path = writer.path().to_owned();
        writer.append(event("record")).unwrap();
        writer.close().unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let final_line = contents.lines().last().unwrap();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{final_line}").unwrap();
        assert!(verify_log(&path, &public_key).is_err());
    }
}

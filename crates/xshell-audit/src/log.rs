use crate::model::{
    AUDIT_FORMAT_VERSION, AuditCheckpoint, AuditEvent, AuditLogEntry, AuditRecord, AuditRecordBody,
    CheckpointBody, WitnessCommitment,
};
use anyhow::{Context, Result, bail};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
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
            format_version: AUDIT_FORMAT_VERSION,
            session_id: self.session_id.clone(),
            sequence: self.sequence,
            daemon_timestamp_unix_ms: unix_timestamp_ms()?,
            client_uid: self.client_uid,
            previous_hash: self.chain_head.clone(),
            event,
        };
        let record_hash = hash_record_body(&body)?;
        let record = AuditRecord {
            body,
            record_hash: record_hash.clone(),
        };
        self.write_entry(&AuditLogEntry::Record(record.clone()))?;
        self.chain_head = record_hash;

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
            format_version: AUDIT_FORMAT_VERSION,
            session_id: self.session_id.clone(),
            checkpoint_sequence: self.checkpoint_sequence,
            previous_checkpoint_hash: self.checkpoint_head.clone(),
            sequence: self.sequence,
            daemon_timestamp_unix_ms: unix_timestamp_ms()?,
            chain_head: self.chain_head.clone(),
            signing_key_id: self.identity.key_id().to_owned(),
            final_checkpoint,
        };
        let mut nonce = [0_u8; 32];
        OsRng.fill_bytes(&mut nonce);
        let commitment = witness_commitment(&body, &nonce)?;
        let signature = self
            .identity
            .signing_key
            .sign(&checkpoint_message(&body, &commitment)?);
        let mut checkpoint = AuditCheckpoint {
            body,
            blinding_nonce: hex::encode(nonce),
            witness: WitnessCommitment {
                scheme: "sha256-blinded-v1".into(),
                commitment,
            },
            signature: hex::encode(signature.to_bytes()),
            checkpoint_hash: String::new(),
        };
        checkpoint.checkpoint_hash = hash_checkpoint(&checkpoint)?;
        self.write_entry(&AuditLogEntry::Checkpoint(checkpoint.clone()))?;
        append_checkpoint_index(&mut self.checkpoint_index, &checkpoint)?;
        self.checkpoint_head = checkpoint.checkpoint_hash.clone();
        Ok(checkpoint)
    }

    fn write_entry(&mut self, entry: &AuditLogEntry) -> Result<()> {
        serde_json::to_writer(&mut self.log, entry)?;
        self.log.write_all(b"\n")?;
        self.log.flush()?;
        self.log.get_ref().sync_data()?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationReport {
    pub session_id: String,
    pub records: u64,
    pub checkpoints: u64,
    pub final_checkpoint: bool,
    pub chain_head: String,
}

pub fn verify_log(path: &Path, public_key_path: &Path) -> Result<VerificationReport> {
    let public_hex = fs::read_to_string(public_key_path)
        .with_context(|| format!("cannot read public key {}", public_key_path.display()))?;
    let public_bytes = hex::decode(public_hex.trim()).context("invalid public key encoding")?;
    let public_bytes: [u8; 32] = public_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("audit public key must contain 32 bytes"))?;
    let verifying_key =
        VerifyingKey::from_bytes(&public_bytes).context("invalid Ed25519 public key")?;
    let expected_key_id = hex::encode(&Sha256::digest(public_bytes)[..16]);

    let reader = BufReader::new(
        File::open(path).with_context(|| format!("cannot open audit log {}", path.display()))?,
    );
    let mut session_id: Option<String> = None;
    let mut expected_sequence = 1_u64;
    let mut chain_head = "0".repeat(64);
    let mut checkpoints = 0_u64;
    let mut checkpoint_head = "0".repeat(64);
    let mut final_checkpoint = false;

    for (line_number, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("cannot read line {}", line_number + 1))?;
        let entry: AuditLogEntry = serde_json::from_str(&line)
            .with_context(|| format!("invalid audit entry on line {}", line_number + 1))?;
        match entry {
            AuditLogEntry::Record(record) => {
                if record.body.format_version != AUDIT_FORMAT_VERSION {
                    bail!("unsupported audit format on line {}", line_number + 1);
                }
                match &session_id {
                    Some(id) if id != &record.body.session_id => {
                        bail!("session ID changed on line {}", line_number + 1)
                    }
                    None => session_id = Some(record.body.session_id.clone()),
                    _ => {}
                }
                if record.body.sequence != expected_sequence {
                    bail!("sequence gap on line {}", line_number + 1);
                }
                if record.body.previous_hash != chain_head {
                    bail!("previous hash mismatch on line {}", line_number + 1);
                }
                let calculated = hash_record_body(&record.body)?;
                if record.record_hash != calculated {
                    bail!("record hash mismatch on line {}", line_number + 1);
                }
                chain_head = calculated;
                expected_sequence += 1;
            }
            AuditLogEntry::Checkpoint(checkpoint) => {
                let expected_session = session_id
                    .get_or_insert_with(|| checkpoint.body.session_id.clone())
                    .clone();
                if checkpoint.body.session_id != expected_session
                    || checkpoint.body.checkpoint_sequence != checkpoints + 1
                    || checkpoint.body.previous_checkpoint_hash != checkpoint_head
                    || checkpoint.body.sequence != expected_sequence - 1
                    || checkpoint.body.chain_head != chain_head
                    || checkpoint.body.signing_key_id != expected_key_id
                {
                    bail!("checkpoint state mismatch on line {}", line_number + 1);
                }
                verify_checkpoint(&checkpoint, &verifying_key)
                    .with_context(|| format!("invalid checkpoint on line {}", line_number + 1))?;
                checkpoint_head = checkpoint.checkpoint_hash.clone();
                checkpoints += 1;
                final_checkpoint = checkpoint.body.final_checkpoint;
            }
        }
    }

    Ok(VerificationReport {
        session_id: session_id.context("audit log contains no records")?,
        records: expected_sequence - 1,
        checkpoints,
        final_checkpoint,
        chain_head,
    })
}

fn verify_checkpoint(checkpoint: &AuditCheckpoint, key: &VerifyingKey) -> Result<()> {
    let nonce = hex::decode(&checkpoint.blinding_nonce).context("invalid checkpoint nonce")?;
    let expected_commitment = witness_commitment(&checkpoint.body, &nonce)?;
    if checkpoint.witness.scheme != "sha256-blinded-v1"
        || checkpoint.witness.commitment != expected_commitment
    {
        bail!("witness commitment mismatch");
    }
    if checkpoint.checkpoint_hash != hash_checkpoint(checkpoint)? {
        bail!("checkpoint hash mismatch");
    }
    let signature_bytes =
        hex::decode(&checkpoint.signature).context("invalid signature encoding")?;
    let signature = Signature::from_slice(&signature_bytes).context("invalid Ed25519 signature")?;
    key.verify(
        &checkpoint_message(&checkpoint.body, &checkpoint.witness.commitment)?,
        &signature,
    )
    .context("checkpoint signature verification failed")
}

fn hash_checkpoint(checkpoint: &AuditCheckpoint) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(CHECKPOINT_HASH_DOMAIN);
    hasher.update(serde_json::to_vec(&checkpoint.body)?);
    hasher.update(checkpoint.blinding_nonce.as_bytes());
    hasher.update(checkpoint.witness.scheme.as_bytes());
    hasher.update(checkpoint.witness.commitment.as_bytes());
    hasher.update(checkpoint.signature.as_bytes());
    Ok(hex::encode(hasher.finalize()))
}

fn hash_record_body(body: &AuditRecordBody) -> Result<String> {
    let encoded = serde_json::to_vec(body)?;
    let mut hasher = Sha256::new();
    hasher.update(RECORD_DOMAIN);
    hasher.update(encoded);
    Ok(hex::encode(hasher.finalize()))
}

fn witness_commitment(body: &CheckpointBody, nonce: &[u8]) -> Result<String> {
    let encoded = serde_json::to_vec(body)?;
    let mut hasher = Sha256::new();
    hasher.update(WITNESS_DOMAIN);
    hasher.update(encoded);
    hasher.update(nonce);
    Ok(hex::encode(hasher.finalize()))
}

fn checkpoint_message(body: &CheckpointBody, commitment: &str) -> Result<Vec<u8>> {
    let encoded = serde_json::to_vec(body)?;
    let mut message =
        Vec::with_capacity(CHECKPOINT_DOMAIN.len() + encoded.len() + commitment.len());
    message.extend_from_slice(CHECKPOINT_DOMAIN);
    message.extend_from_slice(&encoded);
    message.extend_from_slice(commitment.as_bytes());
    Ok(message)
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

fn append_checkpoint_index(file: &mut File, checkpoint: &AuditCheckpoint) -> Result<()> {
    let mut encoded = serde_json::to_vec(checkpoint)?;
    encoded.push(b'\n');
    let descriptor = file.as_raw_fd();
    if unsafe { libc::flock(descriptor, libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot lock checkpoint index");
    }
    let result = (|| {
        file.write_all(&encoded)?;
        file.sync_data()?;
        Ok(())
    })();
    let unlock_result = unsafe { libc::flock(descriptor, libc::LOCK_UN) };
    if unlock_result != 0 && result.is_ok() {
        return Err(std::io::Error::last_os_error()).context("cannot unlock checkpoint index");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn event(text: &str) -> AuditEvent {
        AuditEvent::Input {
            route: "agent".into(),
            text: text.into(),
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

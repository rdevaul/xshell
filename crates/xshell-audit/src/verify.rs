//! Verification of audit logs.
//!
//! From format 10 on, record hashes, checkpoint hashes and checkpoint
//! signatures cover the body bytes exactly as the line stores them. Formats 1
//! to 9 hashed a serialization of the body, and the verifier reproduces it by
//! encoding the parsed body again. Every line of every format is parsed
//! strictly first: a duplicated object key at any depth, or a field the
//! format does not define, is an error.

use crate::log::{
    EntryKind, StoredCheckpoint, StoredRecord, WITNESS_SCHEME, checkpoint_hash, checkpoint_message,
    encode_record_body, record_hash, witness_commitment,
};
use crate::model::{
    AUDIT_FORMAT_VERSION, AuditRecordBody, CheckpointBody, MIN_SUPPORTED_AUDIT_FORMAT_VERSION,
    STORED_BYTES_AUDIT_FORMAT_VERSION,
};
use anyhow::{Context, Result, bail};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// How a log's hashes and signatures relate to its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashingScheme {
    /// Format 10 and later: hashes and signatures cover the stored bytes.
    StoredBytes,
    /// Formats 1 to 9: hashes and signatures cover a serialization of the
    /// parsed body, reproduced by encoding it again. They authenticate the
    /// parsed content, not its exact encoding.
    LegacyReencoded,
}

impl fmt::Display for HashingScheme {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::StoredBytes => "stored-bytes",
            Self::LegacyReencoded => "legacy-reencoded",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationReport {
    pub session_id: String,
    pub format_version: u32,
    pub hashing: HashingScheme,
    pub records: u64,
    pub checkpoints: u64,
    /// The last entry is a final checkpoint. Nothing may follow one.
    pub final_checkpoint: bool,
    /// Records covered by the last signed checkpoint.
    pub signed_records: u64,
    /// Records after the last signed checkpoint. Only the unkeyed hash chain
    /// covers them, so anyone who can write the file can change or add them.
    pub unsigned_records: u64,
    pub chain_head: String,
    /// The log's last checkpoint matched the checkpoint index.
    pub index_checked: bool,
}

/// What a log must be bound to, beyond its own contents.
#[derive(Debug, Clone, Default)]
pub struct VerifyOptions {
    /// The audit session the log must belong to.
    pub session_id: Option<String>,
    /// A `checkpoints.jsonl` whose latest checkpoint for the session must be
    /// the log's last checkpoint.
    pub index: Option<PathBuf>,
}

/// The audit session a log file is named for: `sessions/<id>.jsonl`.
pub fn session_id_from_path(path: &Path) -> Option<String> {
    path.file_stem()?.to_str().map(str::to_owned)
}

/// Verify a log's contents: its chain, its checkpoints and their signatures.
pub fn verify_log(path: &Path, public_key_path: &Path) -> Result<VerificationReport> {
    verify_log_with(path, public_key_path, &VerifyOptions::default())
}

/// Verify a log's contents, then bind it to the session and index in
/// `options`.
pub fn verify_log_with(
    path: &Path,
    public_key_path: &Path,
    options: &VerifyOptions,
) -> Result<VerificationReport> {
    let key = TrustedKey::load(public_key_path)?;
    let reader = BufReader::new(
        File::open(path).with_context(|| format!("cannot open audit log {}", path.display()))?,
    );
    let mut session_id: Option<String> = None;
    let mut format: Option<u32> = None;
    let mut expected_sequence = 1_u64;
    let mut chain_head = "0".repeat(64);
    let mut checkpoints = 0_u64;
    let mut checkpoint_head = "0".repeat(64);
    let mut signed_records = 0_u64;
    let mut final_checkpoint = false;
    let mut last_checkpoint: Option<(u64, String)> = None;

    for (index, line) in reader.lines().enumerate() {
        let line_number = index + 1;
        let line = line.with_context(|| format!("cannot read line {line_number}"))?;
        if final_checkpoint {
            bail!("entry after the final checkpoint on line {line_number}");
        }
        reject_duplicate_keys(&line)
            .with_context(|| format!("invalid audit entry on line {line_number}"))?;
        let tag: EntryTag = serde_json::from_str(&line)
            .with_context(|| format!("invalid audit entry on line {line_number}"))?;
        match tag.entry {
            EntryKind::Record => {
                let stored: StoredRecord<'_> = serde_json::from_str(&line)
                    .with_context(|| format!("invalid audit record on line {line_number}"))?;
                let body: AuditRecordBody = serde_json::from_str(stored.body.get())
                    .with_context(|| format!("invalid audit record on line {line_number}"))?;
                check_format(&mut format, body.format_version, line_number)?;
                match &session_id {
                    Some(id) if id != &body.session_id => {
                        bail!("session ID changed on line {line_number}")
                    }
                    None => session_id = Some(body.session_id.clone()),
                    _ => {}
                }
                if body.sequence != expected_sequence {
                    bail!("sequence gap on line {line_number}");
                }
                if body.previous_hash != chain_head {
                    bail!("previous hash mismatch on line {line_number}");
                }
                let calculated = if stores_bytes(body.format_version) {
                    record_hash(stored.body.get().as_bytes())
                } else {
                    record_hash(encode_record_body(&body)?.as_bytes())
                };
                if stored.record_hash != calculated {
                    bail!("record hash mismatch on line {line_number}");
                }
                chain_head = calculated;
                expected_sequence += 1;
            }
            EntryKind::Checkpoint => {
                let stored: StoredCheckpoint<'_> = serde_json::from_str(&line)
                    .with_context(|| format!("invalid checkpoint on line {line_number}"))?;
                let body: CheckpointBody = serde_json::from_str(stored.body.get())
                    .with_context(|| format!("invalid checkpoint on line {line_number}"))?;
                check_format(&mut format, body.format_version, line_number)?;
                let expected_session = session_id
                    .get_or_insert_with(|| body.session_id.clone())
                    .clone();
                if body.session_id != expected_session
                    || body.checkpoint_sequence != checkpoints + 1
                    || body.previous_checkpoint_hash != checkpoint_head
                    || body.sequence != expected_sequence - 1
                    || body.chain_head != chain_head
                    || body.signing_key_id != key.key_id
                {
                    bail!("checkpoint state mismatch on line {line_number}");
                }
                verify_checkpoint(&stored, &body, &key)
                    .with_context(|| format!("invalid checkpoint on line {line_number}"))?;
                checkpoint_head = stored.checkpoint_hash.clone();
                checkpoints += 1;
                signed_records = body.sequence;
                final_checkpoint = body.final_checkpoint;
                last_checkpoint = Some((body.checkpoint_sequence, stored.checkpoint_hash));
            }
        }
    }

    let session_id = session_id.context("audit log contains no records")?;
    let format_version = format.context("audit log contains no records")?;
    if let Some(expected) = &options.session_id
        && expected != &session_id
    {
        bail!("the log holds audit session {session_id}, not the expected {expected}");
    }
    if let Some(index) = &options.index {
        check_index(index, &session_id, last_checkpoint.as_ref(), &key)
            .with_context(|| format!("checkpoint index {} disagrees", index.display()))?;
    }
    let records = expected_sequence - 1;
    Ok(VerificationReport {
        session_id,
        format_version,
        hashing: if stores_bytes(format_version) {
            HashingScheme::StoredBytes
        } else {
            HashingScheme::LegacyReencoded
        },
        records,
        checkpoints,
        final_checkpoint,
        signed_records,
        unsigned_records: records - signed_records,
        chain_head,
        index_checked: options.index.is_some(),
    })
}

#[derive(serde::Deserialize)]
struct EntryTag {
    entry: EntryKind,
}

struct TrustedKey {
    key: VerifyingKey,
    key_id: String,
}

impl TrustedKey {
    fn load(path: &Path) -> Result<Self> {
        let public_hex = fs::read_to_string(path)
            .with_context(|| format!("cannot read public key {}", path.display()))?;
        let public_bytes = hex::decode(public_hex.trim()).context("invalid public key encoding")?;
        let public_bytes: [u8; 32] = public_bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("audit public key must contain 32 bytes"))?;
        Ok(Self {
            key: VerifyingKey::from_bytes(&public_bytes).context("invalid Ed25519 public key")?,
            key_id: hex::encode(&Sha256::digest(public_bytes)[..16]),
        })
    }
}

fn supported_format(version: u32) -> bool {
    (MIN_SUPPORTED_AUDIT_FORMAT_VERSION..=AUDIT_FORMAT_VERSION).contains(&version)
}

fn stores_bytes(version: u32) -> bool {
    version >= STORED_BYTES_AUDIT_FORMAT_VERSION
}

/// Every entry of a log has its writer's format. A log that mixes formats
/// could otherwise move a stored-bytes record onto the legacy path.
fn check_format(format: &mut Option<u32>, version: u32, line_number: usize) -> Result<()> {
    if !supported_format(version) {
        bail!("unsupported audit format {version} on line {line_number}");
    }
    match format {
        Some(format) if *format != version => {
            bail!("audit format changed from {format} to {version} on line {line_number}")
        }
        Some(_) => {}
        None => *format = Some(version),
    }
    Ok(())
}

fn verify_checkpoint(
    stored: &StoredCheckpoint<'_>,
    body: &CheckpointBody,
    key: &TrustedKey,
) -> Result<()> {
    let legacy;
    let bytes = if stores_bytes(body.format_version) {
        stored.body.get().as_bytes()
    } else {
        legacy = serde_json::to_vec(body)?;
        &legacy
    };
    let nonce = hex::decode(&stored.blinding_nonce).context("invalid checkpoint nonce")?;
    if stored.witness.scheme != WITNESS_SCHEME
        || stored.witness.commitment != witness_commitment(bytes, &nonce)
    {
        bail!("witness commitment mismatch");
    }
    if stored.checkpoint_hash
        != checkpoint_hash(
            bytes,
            &stored.blinding_nonce,
            &stored.witness,
            &stored.signature,
        )
    {
        bail!("checkpoint hash mismatch");
    }
    let signature_bytes = hex::decode(&stored.signature).context("invalid signature encoding")?;
    let signature = Signature::from_slice(&signature_bytes).context("invalid Ed25519 signature")?;
    key.key
        .verify_strict(
            &checkpoint_message(bytes, &stored.witness.commitment),
            &signature,
        )
        .context("checkpoint signature verification failed")
}

/// Check the log's last checkpoint against the latest signed checkpoint the
/// index holds for the session. A log rolled back to an earlier checkpoint,
/// cut before its first one, or swapped for another copy disagrees with it.
fn check_index(
    index: &Path,
    session_id: &str,
    last_checkpoint: Option<&(u64, String)>,
    key: &TrustedKey,
) -> Result<()> {
    let reader = BufReader::new(File::open(index).context("cannot open the checkpoint index")?);
    let mut latest: Option<(u64, String)> = None;
    for (position, line) in reader.lines().enumerate() {
        let line_number = position + 1;
        let line = line.with_context(|| format!("cannot read index line {line_number}"))?;
        reject_duplicate_keys(&line)
            .with_context(|| format!("invalid index entry on line {line_number}"))?;
        let stored: StoredCheckpoint<'_> = serde_json::from_str(&line)
            .with_context(|| format!("invalid index entry on line {line_number}"))?;
        if stored.entry.is_some() {
            bail!("invalid index entry on line {line_number}: unexpected `entry`");
        }
        let body: CheckpointBody = serde_json::from_str(stored.body.get())
            .with_context(|| format!("invalid index entry on line {line_number}"))?;
        if body.session_id != session_id {
            continue;
        }
        if !supported_format(body.format_version) || body.signing_key_id != key.key_id {
            bail!("index entry on line {line_number} cannot be verified with this key");
        }
        verify_checkpoint(&stored, &body, key)
            .with_context(|| format!("invalid index entry on line {line_number}"))?;
        match &latest {
            Some((sequence, hash)) if *sequence == body.checkpoint_sequence => {
                if *hash != stored.checkpoint_hash {
                    bail!(
                        "the index holds two different checkpoints {sequence} for session {session_id}"
                    );
                }
            }
            Some((sequence, _)) if *sequence > body.checkpoint_sequence => {}
            _ => latest = Some((body.checkpoint_sequence, stored.checkpoint_hash)),
        }
    }
    let Some((indexed_sequence, indexed_hash)) = latest else {
        bail!("the index has no checkpoint for session {session_id}");
    };
    let Some((log_sequence, log_hash)) = last_checkpoint else {
        bail!(
            "the index holds checkpoint {indexed_sequence} for this session but the log has none: \
             the log was truncated or replaced"
        );
    };
    if indexed_sequence > *log_sequence {
        bail!(
            "the index holds checkpoint {indexed_sequence} for this session but the log ends at \
             checkpoint {log_sequence}: the log was truncated or rolled back"
        );
    }
    if indexed_sequence < *log_sequence {
        bail!(
            "the log holds checkpoint {log_sequence} but the index ends at checkpoint \
             {indexed_sequence} for this session: the index was truncated or rolled back"
        );
    }
    if indexed_hash != *log_hash {
        bail!("checkpoint {log_sequence} differs from the one in the index: the log was replaced");
    }
    Ok(())
}

/// Reject a JSON document that repeats a key in any object. Typed parsing
/// rejects a repeated struct field, but a free-form value such as tool
/// arguments keeps the last of two keys silently, so `"command":"ls",
/// "command":"rm -rf ~"` would read differently to different parsers.
pub(crate) fn reject_duplicate_keys(line: &str) -> Result<()> {
    let mut deserializer = serde_json::Deserializer::from_str(line);
    UniqueKeys::deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(())
}

struct UniqueKeys;

impl<'de> Deserialize<'de> for UniqueKeys {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(UniqueKeysVisitor)
    }
}

struct UniqueKeysVisitor;

impl<'de> Visitor<'de> for UniqueKeysVisitor {
    type Value = UniqueKeys;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, _: bool) -> Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }

    fn visit_i64<E>(self, _: i64) -> Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }

    fn visit_u64<E>(self, _: u64) -> Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }

    fn visit_f64<E>(self, _: f64) -> Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }

    fn visit_str<E>(self, _: &str) -> Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }

    fn visit_unit<E>(self) -> Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<UniqueKeys, A::Error> {
        while seq.next_element::<UniqueKeys>()?.is_some() {}
        Ok(UniqueKeys)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<UniqueKeys, A::Error> {
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom(format_args!("duplicate key {key:?}")));
            }
            map.next_value::<UniqueKeys>()?;
        }
        Ok(UniqueKeys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{AuditLogWriter, SigningIdentity};
    use crate::model::{AuditEvent, AuditLogEntry};
    use crate::protocol::ClientRequest;
    use std::io::Write;
    use tempfile::TempDir;

    const ZERO: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn input(text: &str) -> AuditEvent {
        AuditEvent::Input {
            route: "agent".into(),
            text: text.into(),
            images: Vec::new(),
        }
    }

    struct Fixture {
        _temp: TempDir,
        directory: PathBuf,
        public_key: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let directory = temp.path().to_owned();
            SigningIdentity::load_or_create(&directory).unwrap();
            Self {
                public_key: directory.join("signing-key.pub"),
                directory,
                _temp: temp,
            }
        }

        fn writer(&self, interval: u64) -> AuditLogWriter {
            let identity = SigningIdentity::load_or_create(&self.directory).unwrap();
            AuditLogWriter::create(&self.directory, identity, 501, interval).unwrap()
        }

        fn index(&self) -> PathBuf {
            self.directory.join("checkpoints.jsonl")
        }
    }

    /// A record line whose hash is correct for exactly `body`'s bytes.
    fn record_line(body: &str) -> String {
        format!(
            r#"{{"entry":"record","body":{body},"record_hash":"{}"}}"#,
            record_hash(body.as_bytes())
        )
    }

    fn body(session: &str, sequence: u64, previous: &str, event: &str) -> String {
        format!(
            r#"{{"format_version":{AUDIT_FORMAT_VERSION},"session_id":"{session}","sequence":{sequence},"daemon_timestamp_unix_ms":1,"client_uid":501,"previous_hash":"{previous}","event":{event}}}"#
        )
    }

    fn write_lines(fixture: &Fixture, name: &str, lines: &[String]) -> PathBuf {
        let path = fixture.directory.join(format!("{name}.jsonl"));
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    fn error(result: Result<VerificationReport>) -> String {
        format!("{:#}", result.expect_err("verification should fail"))
    }

    #[test]
    fn f14_an_entry_after_the_final_checkpoint_is_rejected() {
        let fixture = Fixture::new();
        let mut writer = fixture.writer(16);
        let path = writer.path().to_owned();
        let session = writer.session_id().to_owned();
        writer.append(input("genuine")).unwrap();
        writer.close().unwrap();
        let report = verify_log(&path, &fixture.public_key).unwrap();
        assert!(report.final_checkpoint);
        assert_eq!(report.unsigned_records, 0);

        // A forged record that extends the chain correctly: it would verify
        // without the finality rule.
        let forged = record_line(&body(
            &session,
            2,
            &report.chain_head,
            r#"{"type":"input","route":"agent","text":"forged"}"#,
        ));
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{forged}").unwrap();
        let message = error(verify_log(&path, &fixture.public_key));
        assert!(message.contains("after the final checkpoint"), "{message}");
    }

    #[test]
    fn f14_records_after_the_last_checkpoint_are_reported_unsigned() {
        let fixture = Fixture::new();
        let mut writer = fixture.writer(2);
        let path = writer.path().to_owned();
        let session = writer.session_id().to_owned();
        for text in ["one", "two", "three"] {
            writer.append(input(text)).unwrap();
        }
        drop(writer);
        let report = verify_log(&path, &fixture.public_key).unwrap();
        assert_eq!(
            (
                report.records,
                report.signed_records,
                report.unsigned_records
            ),
            (3, 2, 1)
        );
        assert!(!report.final_checkpoint);

        // The unsigned tail is only chained: an appended record verifies, and
        // the report says it is not covered by a signature.
        let forged = record_line(&body(
            &session,
            4,
            &report.chain_head,
            r#"{"type":"input","route":"agent","text":"appended"}"#,
        ));
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{forged}").unwrap();
        let report = verify_log(&path, &fixture.public_key).unwrap();
        assert_eq!((report.signed_records, report.unsigned_records), (2, 2));
    }

    #[test]
    fn f14_a_non_round_tripping_float_verifies_under_format_10() {
        let fixture = Fixture::new();
        let mut writer = fixture.writer(1);
        let path = writer.path().to_owned();
        writer
            .append(AuditEvent::ToolRequested {
                call_id: "c1".into(),
                name: "run_shell".into(),
                arguments: serde_json::json!({
                    "command": "sleep 1",
                    "timeout": 1.0715660391465826e-75,
                    "values": [5e-324, 1.7976931348623157e308, 0.1, 123456789.98765433],
                }),
            })
            .unwrap();
        writer.close().unwrap();
        let report = verify_log(&path, &fixture.public_key).unwrap();
        assert_eq!(report.format_version, AUDIT_FORMAT_VERSION);
        assert_eq!(report.hashing, HashingScheme::StoredBytes);
        assert!(report.final_checkpoint);
    }

    #[test]
    fn f14_hashes_cover_the_stored_bytes_not_a_re_encoding() {
        let fixture = Fixture::new();
        // An encoding no serde_json writer produces: escapes, spacing, key
        // order and a non-shortest number. Its hash is over these bytes, so it
        // verifies as stored.
        let event = r#"{ "name" : "run_\u0073hell", "call_id":"c", "type":"tool_requested", "arguments":{"timeout":1.50e0,"b":1,"a":2} }"#;
        let first = body("s", 1, ZERO, event);
        assert_ne!(
            encode_record_body(&serde_json::from_str(&first).unwrap()).unwrap(),
            first
        );
        let path = write_lines(&fixture, "s", &[record_line(&first)]);
        let report = verify_log(&path, &fixture.public_key).unwrap();
        assert_eq!(report.unsigned_records, 1);

        // Changing only the encoding of a genuine record breaks its hash.
        let mut writer = fixture.writer(16);
        let path = writer.path().to_owned();
        writer.append(input("hi")).unwrap();
        writer.close().unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        for (original, variant) in [
            (r#""text":"hi""#, r#""text":"\u0068i""#),
            (
                r#""route":"agent","text":"hi""#,
                r#""text":"hi","route":"agent""#,
            ),
            (r#""text":"hi""#, r#""text": "hi""#),
        ] {
            assert!(contents.contains(original), "{original}");
            fs::write(&path, contents.replacen(original, variant, 1)).unwrap();
            let message = error(verify_log(&path, &fixture.public_key));
            assert!(
                message.contains("record hash mismatch"),
                "{variant}: {message}"
            );
        }
    }

    #[test]
    fn f14_duplicate_keys_are_rejected_at_any_depth() {
        let fixture = Fixture::new();
        let nested = body(
            "s",
            1,
            ZERO,
            r#"{"type":"tool_requested","call_id":"c","name":"run_shell","arguments":{"command":"ls","command":"rm -rf ~"}}"#,
        );
        let escaped = body(
            "s",
            1,
            ZERO,
            r#"{"type":"tool_requested","call_id":"c","name":"run_shell","arguments":{"a":{"x":1,"\u0078":2}}}"#,
        );
        let in_body = body(
            "s",
            1,
            ZERO,
            r#"{"type":"input","route":"agent","text":"a"}"#,
        )
        .replacen(r#""sequence":1,"#, r#""sequence":1,"sequence":1,"#, 1);
        let genuine = body(
            "s",
            1,
            ZERO,
            r#"{"type":"input","route":"agent","text":"a"}"#,
        );
        let at_line = record_line(&genuine).replacen(
            r#""entry":"record","#,
            r#""entry":"record","entry":"record","#,
            1,
        );
        for (name, line) in [
            ("nested", record_line(&nested)),
            ("escaped", record_line(&escaped)),
            ("body", record_line(&in_body)),
            ("line", at_line),
        ] {
            let path = write_lines(&fixture, name, &[line]);
            let message = error(verify_log(&path, &fixture.public_key));
            assert!(message.contains("duplicate key"), "{name}: {message}");
        }
        verify_log(
            &write_lines(&fixture, "genuine", &[record_line(&genuine)]),
            &fixture.public_key,
        )
        .unwrap();
    }

    #[test]
    fn f14_unknown_fields_are_rejected() {
        let fixture = Fixture::new();
        let event = r#"{"type":"input","route":"agent","text":"a"}"#;
        let in_event = body(
            "s",
            1,
            ZERO,
            r#"{"type":"input","route":"agent","text":"a","sudo":true}"#,
        );
        let in_body = body("s", 1, ZERO, event).replacen(
            r#""client_uid":501"#,
            r#""client_uid":501,"real_uid":0"#,
            1,
        );
        let in_line = record_line(&body("s", 1, ZERO, event)).replacen(
            r#""entry":"record","#,
            r#""entry":"record","note":"x","#,
            1,
        );
        for (name, line) in [
            ("event", record_line(&in_event)),
            ("body", record_line(&in_body)),
            ("line", in_line),
        ] {
            let path = write_lines(&fixture, name, &[line]);
            let message = error(verify_log(&path, &fixture.public_key));
            assert!(message.contains("unknown field"), "{name}: {message}");
        }

        // A checkpoint with an extra field.
        let mut writer = fixture.writer(16);
        let path = writer.path().to_owned();
        writer.append(input("a")).unwrap();
        writer.close().unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        fs::write(
            &path,
            contents.replacen(
                r#""final_checkpoint":true"#,
                r#""final_checkpoint":true,"x":1"#,
                1,
            ),
        )
        .unwrap();
        let message = error(verify_log(&path, &fixture.public_key));
        assert!(message.contains("unknown field"), "{message}");
    }

    #[test]
    fn f14_typed_audit_values_reject_unknown_fields() {
        // `deny_unknown_fields` on internally tagged enums: struct variants
        // and newtype variants both refuse extras, and the tag is accepted.
        let event = r#"{"type":"session_ended","reason":"exit"}"#;
        serde_json::from_str::<AuditEvent>(event).unwrap();
        assert!(
            serde_json::from_str::<AuditEvent>(r#"{"type":"session_ended","reason":"exit","x":1}"#)
                .is_err()
        );
        let request = r#"{"request":"open","protocol_version":11,"client_version":"v"}"#;
        serde_json::from_str::<ClientRequest>(request).unwrap();
        assert!(
            serde_json::from_str::<ClientRequest>(&request.replace("}", r#","x":1}"#)).is_err()
        );
        let entry = format!(
            r#"{{"entry":"record","body":{},"record_hash":"00"}}"#,
            body("s", 1, ZERO, event)
        );
        serde_json::from_str::<AuditLogEntry>(&entry).unwrap();
        assert!(
            serde_json::from_str::<AuditLogEntry>(&entry.replacen(
                r#""record_hash""#,
                r#""x":1,"record_hash""#,
                1
            ))
            .is_err()
        );
    }

    #[test]
    fn f14_a_log_for_another_session_is_rejected() {
        let fixture = Fixture::new();
        let mut first = fixture.writer(16);
        let first_path = first.path().to_owned();
        first.append(input("first")).unwrap();
        first.close().unwrap();
        let mut second = fixture.writer(16);
        let second_path = second.path().to_owned();
        let second_id = second.session_id().to_owned();
        second.append(input("second")).unwrap();
        second.close().unwrap();

        let expected = |path: &Path| VerifyOptions {
            session_id: session_id_from_path(path),
            index: Some(fixture.index()),
        };
        verify_log_with(&first_path, &fixture.public_key, &expected(&first_path)).unwrap();

        // Replace the first session's log with the second, cleanly closed.
        fs::copy(&second_path, &first_path).unwrap();
        verify_log(&first_path, &fixture.public_key).unwrap();
        let message = error(verify_log_with(
            &first_path,
            &fixture.public_key,
            &expected(&first_path),
        ));
        assert!(message.contains(&second_id), "{message}");
        assert!(message.contains("not the expected"), "{message}");
    }

    #[test]
    fn f14_the_index_detects_a_rolled_back_log() {
        let fixture = Fixture::new();
        let mut writer = fixture.writer(1);
        let path = writer.path().to_owned();
        writer.append(input("one")).unwrap();
        writer.append(input("two")).unwrap();
        writer.close().unwrap();
        let options = VerifyOptions {
            session_id: session_id_from_path(&path),
            index: Some(fixture.index()),
        };
        let report = verify_log_with(&path, &fixture.public_key, &options).unwrap();
        assert!(report.index_checked);

        // Roll back to the first checkpoint: a valid prefix on its own.
        let contents = fs::read_to_string(&path).unwrap();
        let prefix: Vec<String> = contents.lines().take(2).map(str::to_owned).collect();
        fs::write(&path, prefix.join("\n") + "\n").unwrap();
        verify_log(&path, &fixture.public_key).unwrap();
        let message = error(verify_log_with(&path, &fixture.public_key, &options));
        assert!(message.contains("truncated or rolled back"), "{message}");

        // Cut before the first checkpoint.
        fs::write(&path, contents.lines().next().unwrap().to_owned() + "\n").unwrap();
        let message = error(verify_log_with(&path, &fixture.public_key, &options));
        assert!(message.contains("the log has none"), "{message}");

        // A session the index has never seen.
        let lines = [record_line(&body(
            "unknown",
            1,
            ZERO,
            r#"{"type":"input","route":"agent","text":"a"}"#,
        ))];
        let unknown = write_lines(&fixture, "unknown", &lines);
        let message = error(verify_log_with(
            &unknown,
            &fixture.public_key,
            &VerifyOptions {
                session_id: None,
                index: Some(fixture.index()),
            },
        ));
        assert!(message.contains("no checkpoint for session"), "{message}");
    }

    #[test]
    fn f14_a_log_may_not_mix_formats() {
        let fixture = Fixture::new();
        let event = r#"{"type":"input","route":"agent","text":"a"}"#;
        let first = body("s", 1, ZERO, event);
        let first_hash = record_hash(first.as_bytes());
        let second = body("s", 2, &first_hash, event).replacen(
            &format!(r#""format_version":{AUDIT_FORMAT_VERSION}"#),
            r#""format_version":9"#,
            1,
        );
        let path = write_lines(&fixture, "s", &[record_line(&first), record_line(&second)]);
        let message = error(verify_log(&path, &fixture.public_key));
        assert!(message.contains("format changed"), "{message}");
    }

    #[test]
    fn f14_format_2_session_attachments_omit_terminal_stream() {
        // Byte-for-byte what format 1 and 2 writers produced, and what format
        // 3 and later writers produce for the same event.
        let old = r#"{"format_version":2,"session_id":"s","sequence":2,"daemon_timestamp_unix_ms":1,"client_uid":501,"previous_hash":"00","event":{"type":"logical_session_attached","action":"created","session_id":"x","name":"work","host_id":"h","host_alias":"local","user":"op"}}"#;
        let body: AuditRecordBody = serde_json::from_str(old).unwrap();
        assert_eq!(encode_record_body(&body).unwrap(), old);
        let later = old
            .replace(r#""format_version":2"#, r#""format_version":3"#)
            .replace(r#""user":"op"}"#, r#""user":"op","terminal_stream":null}"#);
        let body: AuditRecordBody = serde_json::from_str(&later).unwrap();
        assert_eq!(encode_record_body(&body).unwrap(), later);
    }
}

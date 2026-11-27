//! Verification tests against real signed fixtures (see
//! `tests/fixtures/README.md`), with one tampering test per rule of
//! docs/signed-skills.md §3.3 and §4.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha512};
use tempfile::TempDir;
use xshell_skills::{
    MANIFEST_NAME, RevokedKeys, SIGNATURE_NAME, SkillError, SkillNetwork, Trust, TrustedSigners,
    VerifiedManifest, verify_manifest, verify_skill,
};

const SIGNER: &str = "fixture-signer@example.com";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fixture(name: &str) -> Vec<u8> {
    fs::read(fixtures().join(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
}

fn fixture_text(name: &str) -> String {
    String::from_utf8(fixture(name)).unwrap()
}

/// 2026-11-27T00:00:00Z.
fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_795_737_600)
}

fn verify_with(dir: &Path, signers: &str, revoked: &str) -> Result<VerifiedManifest, SkillError> {
    let signers = TrustedSigners::parse(signers).expect("trusted signers");
    let revoked = RevokedKeys::parse(revoked).expect("revoked keys");
    verify_skill(
        dir,
        Trust {
            signers: &signers,
            revoked: &revoked,
            now: now(),
        },
    )
}

fn verify(dir: &Path) -> Result<VerifiedManifest, SkillError> {
    verify_with(dir, &fixture_text("allowed_signers"), "")
}

fn verify_bytes(manifest: &[u8], signature: &[u8]) -> Result<VerifiedManifest, SkillError> {
    let signers = TrustedSigners::parse(&fixture_text("allowed_signers")).unwrap();
    let revoked = RevokedKeys::default();
    verify_manifest(
        manifest,
        signature,
        Trust {
            signers: &signers,
            revoked: &revoked,
            now: now(),
        },
    )
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A writable copy of a fixture skill.
fn copy_of(name: &str) -> (TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("skill");
    copy_tree(&fixtures().join(name), &dir);
    (temp, dir)
}

#[track_caller]
fn refused(result: Result<VerifiedManifest, SkillError>, expected: &str) -> SkillError {
    let error = result.expect_err("the skill should have been refused");
    assert!(
        error.to_string().contains(expected),
        "expected {expected:?} in {error}"
    );
    error
}

// ------------------------------------------------- a signer for the tests
//
// Rules about the manifest's content can only be tested with a manifest that
// breaks the rule and is still validly signed. This signs with a fixed test
// key, writing the same format as `ssh-keygen -Y sign`.

fn test_key() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}

fn put(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn key_blob(key: &SigningKey) -> Vec<u8> {
    let mut blob = Vec::new();
    put(&mut blob, b"ssh-ed25519");
    put(&mut blob, key.verifying_key().as_bytes());
    blob
}

fn armor(blob: &[u8]) -> Vec<u8> {
    let encoded = STANDARD.encode(blob);
    let mut text = String::from("-----BEGIN SSH SIGNATURE-----\n");
    for line in encoded.as_bytes().chunks(70) {
        text.push_str(std::str::from_utf8(line).unwrap());
        text.push('\n');
    }
    text.push_str("-----END SSH SIGNATURE-----\n");
    text.into_bytes()
}

/// The unarmored signature, with `reserved` and trailing bytes for the tests
/// that need them.
fn sign_blob(message: &[u8], namespace: &str, reserved: &[u8], trailing: &[u8]) -> Vec<u8> {
    let key = test_key();
    let mut signed = b"SSHSIG".to_vec();
    put(&mut signed, namespace.as_bytes());
    put(&mut signed, reserved);
    put(&mut signed, b"sha512");
    put(&mut signed, &Sha512::digest(message));
    let mut signature = Vec::new();
    put(&mut signature, b"ssh-ed25519");
    put(&mut signature, &key.sign(&signed).to_bytes());

    let mut blob = b"SSHSIG".to_vec();
    blob.extend_from_slice(&1u32.to_be_bytes());
    put(&mut blob, &key_blob(&key));
    put(&mut blob, namespace.as_bytes());
    put(&mut blob, reserved);
    put(&mut blob, b"sha512");
    put(&mut blob, &signature);
    blob.extend_from_slice(trailing);
    blob
}

fn sign(message: &[u8]) -> Vec<u8> {
    armor(&sign_blob(message, "skill-manifest", b"", b""))
}

fn test_key_line() -> String {
    format!(
        "tester namespaces=\"skill-manifest\" ssh-ed25519 {}\n",
        STANDARD.encode(key_blob(&test_key()))
    )
}

fn verify_test_signed(manifest: &[u8], signature: &[u8]) -> Result<VerifiedManifest, SkillError> {
    let signers = TrustedSigners::parse(&test_key_line()).unwrap();
    let revoked = RevokedKeys::default();
    verify_manifest(
        manifest,
        signature,
        Trust {
            signers: &signers,
            revoked: &revoked,
            now: now(),
        },
    )
}

const GOOD_MANIFEST: &str = r#"{
  "manifest_version": 2,
  "name": "demo",
  "version": "1",
  "description": "A demonstration.",
  "files": {"SKILL.md": {"sha256": "HASH", "size": 5}},
  "scripts": [],
  "requires": {"network": "none", "secrets": [], "read_only": []}
}"#;

fn good_manifest() -> String {
    GOOD_MANIFEST.replace("HASH", &"ab".repeat(32))
}

fn verify_edited(from: &str, to: &str) -> Result<VerifiedManifest, SkillError> {
    let manifest = good_manifest();
    assert!(manifest.contains(from), "{from:?} is not in the manifest");
    let manifest = manifest.replacen(from, to, 1);
    verify_test_signed(manifest.as_bytes(), &sign(manifest.as_bytes()))
}

// --------------------------------------------------------------- fixtures

#[test]
fn a_skill_signed_by_skill_signer_verifies() {
    let verified = verify(&fixtures().join("signed-by-skill-signer")).unwrap();
    assert_eq!(verified.signer, SIGNER);
    assert_eq!(
        verified.manifest_sha256,
        "b721a0d5af184b329d9539327268ac8b54ae3b9188b5e5113aef10f166f13c96"
    );
    assert_eq!(
        verified.key.fingerprint(),
        fixture_text("signer.fingerprint").trim()
    );
    let manifest = verified.manifest;
    assert_eq!(manifest.name, "fixture-skill");
    assert_eq!(manifest.version, "1.2.0");
    assert!(manifest.description.contains('—'));
    assert_eq!(
        manifest.files.keys().collect::<Vec<_>>(),
        [".settings", "SKILL.md", "scripts/hello.sh"]
    );
    assert_eq!(manifest.scripts, ["scripts/hello.sh"]);
    assert!(!manifest.is_instruction_only());
    assert_eq!(manifest.requires.network, SkillNetwork::Loopback);
    assert_eq!(manifest.requires.secrets, ["EXAMPLE_TOKEN"]);
}

#[test]
fn a_skill_signed_by_stock_ssh_keygen_verifies() {
    let verified = verify(&fixtures().join("signed-by-ssh-keygen")).unwrap();
    assert_eq!(verified.signer, SIGNER);
    assert_eq!(verified.manifest.name, "plain");
    assert!(verified.manifest.is_instruction_only());
    assert_eq!(verified.manifest.requires.network, SkillNetwork::None);
}

#[test]
fn a_sha256_signature_verifies() {
    let manifest = fixture("signed-by-ssh-keygen/MANIFEST.json");
    verify_bytes(&manifest, &fixture("signatures/sha256.sig")).unwrap();
}

#[test]
fn a_signature_for_another_namespace_is_refused() {
    let manifest = fixture("signed-by-ssh-keygen/MANIFEST.json");
    let result = verify_bytes(&manifest, &fixture("signatures/wrong-namespace.sig"));
    refused(result, "made for namespace \"file\"");
}

#[test]
fn a_signature_by_an_untrusted_key_is_refused() {
    let manifest = fixture("signed-by-ssh-keygen/MANIFEST.json");
    let result = verify_bytes(&manifest, &fixture("signatures/other-key.sig"));
    let error = refused(result, "does not belong to any trusted signer");
    assert!(matches!(error, SkillError::UntrustedKey { .. }));
}

#[test]
fn rsa_and_ecdsa_signatures_are_refused_as_unsupported() {
    let manifest = fixture("signed-by-ssh-keygen/MANIFEST.json");
    for (file, kind) in [("rsa.sig", "ssh-rsa"), ("ecdsa.sig", "ecdsa-sha2-nistp256")] {
        let result = verify_bytes(&manifest, &fixture(&format!("signatures/{file}")));
        let error = refused(result, kind);
        assert!(matches!(error, SkillError::Unsupported(_)), "{error}");
    }
}

#[test]
fn a_signature_over_another_manifest_is_refused() {
    let manifest = fixture("signed-by-skill-signer/MANIFEST.json");
    let signature = fixture("signed-by-ssh-keygen/MANIFEST.json.sig");
    refused(
        verify_bytes(&manifest, &signature),
        "does not match the signed file",
    );
}

// ------------------------------------------------- tampering with a skill

#[test]
fn an_edited_manifest_is_refused() {
    let (_temp, dir) = copy_of("signed-by-skill-signer");
    let path = dir.join(MANIFEST_NAME);
    let edited = fs::read_to_string(&path).unwrap().replace("1.2.0", "1.2.1");
    fs::write(&path, edited).unwrap();
    refused(verify(&dir), "does not match the signed file");
}

#[test]
fn a_reformatted_manifest_is_refused() {
    // The signature covers the stored bytes, not the JSON value.
    let (_temp, dir) = copy_of("signed-by-skill-signer");
    let path = dir.join(MANIFEST_NAME);
    let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    refused(verify(&dir), "does not match the signed file");
}

#[test]
fn a_changed_file_is_refused() {
    let (_temp, dir) = copy_of("signed-by-skill-signer");
    // Same size, different content.
    fs::write(dir.join("scripts/hello.sh"), "#!/bin/sh\necho HELLO\n").unwrap();
    refused(
        verify(&dir),
        "\"scripts/hello.sh\" does not match the manifest",
    );
}

#[test]
fn a_file_of_another_size_is_refused() {
    let (_temp, dir) = copy_of("signed-by-skill-signer");
    fs::write(dir.join("SKILL.md"), "short").unwrap();
    refused(verify(&dir), "\"SKILL.md\" is 5 bytes");
}

#[test]
fn a_missing_file_is_refused() {
    let (_temp, dir) = copy_of("signed-by-skill-signer");
    fs::remove_file(dir.join(".settings")).unwrap();
    refused(verify(&dir), "listed file \".settings\" is missing");
}

#[test]
fn an_unlisted_file_is_refused_wherever_it_is() {
    for extra in [
        ".env",
        "extra.txt",
        "scripts/.hidden",
        "__pycache__/x.pyc",
        "new/deep/file",
    ] {
        let (_temp, dir) = copy_of("signed-by-skill-signer");
        let path = dir.join(extra);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "unsigned").unwrap();
        refused(
            verify(&dir),
            &format!("{extra:?} is not listed in the manifest"),
        );
    }
}

#[test]
fn a_listed_file_replaced_by_a_link_to_the_same_content_is_refused() {
    let (temp, dir) = copy_of("signed-by-skill-signer");
    let outside = temp.path().join("outside.sh");
    fs::rename(dir.join("scripts/hello.sh"), &outside).unwrap();
    symlink(&outside, dir.join("scripts/hello.sh")).unwrap();
    refused(verify(&dir), "\"scripts/hello.sh\" is a symbolic link");
}

#[test]
fn a_linked_directory_is_refused() {
    let (temp, dir) = copy_of("signed-by-skill-signer");
    let outside = temp.path().join("outside");
    fs::rename(dir.join("scripts"), &outside).unwrap();
    symlink(&outside, dir.join("scripts")).unwrap();
    refused(verify(&dir), "\"scripts\" is a symbolic link");
}

#[test]
fn a_dangling_link_is_refused() {
    let (_temp, dir) = copy_of("signed-by-skill-signer");
    symlink("/nonexistent", dir.join("dangling")).unwrap();
    refused(verify(&dir), "\"dangling\" is a symbolic link");
}

#[test]
fn a_fifo_is_refused() {
    let (_temp, dir) = copy_of("signed-by-skill-signer");
    let path = std::ffi::CString::new(dir.join("pipe").to_str().unwrap()).unwrap();
    // SAFETY: `path` is a valid NUL-terminated string that outlives the call.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    refused(verify(&dir), "\"pipe\" is not a regular file");
}

#[test]
fn a_linked_manifest_or_signature_is_refused() {
    for name in [MANIFEST_NAME, SIGNATURE_NAME] {
        let (temp, dir) = copy_of("signed-by-skill-signer");
        let outside = temp.path().join("moved");
        fs::rename(dir.join(name), &outside).unwrap();
        symlink(&outside, dir.join(name)).unwrap();
        refused(verify(&dir), &format!("cannot open {name}"));
    }
}

#[test]
fn a_missing_signature_is_refused() {
    let (_temp, dir) = copy_of("signed-by-skill-signer");
    fs::remove_file(dir.join(SIGNATURE_NAME)).unwrap();
    refused(verify(&dir), "cannot open MANIFEST.json.sig");
}

#[test]
fn an_oversized_manifest_is_refused_before_anything_else() {
    let (_temp, dir) = copy_of("signed-by-skill-signer");
    fs::write(dir.join(MANIFEST_NAME), vec![b' '; 1024 * 1024 + 1]).unwrap();
    refused(verify(&dir), "MANIFEST.json is larger than");
}

#[test]
fn a_skill_directory_that_is_a_link_is_refused() {
    let (temp, dir) = copy_of("signed-by-skill-signer");
    let link = temp.path().join("link");
    symlink(&dir, &link).unwrap();
    refused(verify(&link), "is not a directory");
}

// ------------------------------------------------------- signature format

#[test]
fn the_test_signer_writes_what_the_verifier_accepts() {
    let manifest = good_manifest();
    let verified = verify_test_signed(manifest.as_bytes(), &sign(manifest.as_bytes())).unwrap();
    assert_eq!(verified.signer, "tester");
}

#[test]
fn malformed_signature_files_are_refused() {
    let manifest = good_manifest();
    let good = String::from_utf8(sign(manifest.as_bytes())).unwrap();
    let blob = sign_blob(manifest.as_bytes(), "skill-manifest", b"", b"");
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("empty", Vec::new(), "BEGIN line"),
        ("not text", vec![0xff, 0xfe], "not text"),
        (
            "no armor",
            STANDARD.encode(&blob).into_bytes(),
            "BEGIN line",
        ),
        (
            "text before the armor",
            format!("hello\n{good}").into_bytes(),
            "BEGIN line",
        ),
        (
            "text after the armor",
            format!("{good}hello\n").into_bytes(),
            "after the END line",
        ),
        (
            "no END line",
            good.replace("-----END SSH SIGNATURE-----\n", "")
                .into_bytes(),
            "END line is missing",
        ),
        (
            "body is not base64",
            b"-----BEGIN SSH SIGNATURE-----\n!!!\n-----END SSH SIGNATURE-----\n".to_vec(),
            "not base64",
        ),
        (
            "wrong magic",
            armor(b"SSHSIX\0\0\0\x01"),
            "does not start with SSHSIG",
        ),
        ("truncated", armor(&blob[..blob.len() - 10]), "ends inside"),
        (
            "trailing bytes",
            armor(&sign_blob(
                manifest.as_bytes(),
                "skill-manifest",
                b"",
                b"\0",
            )),
            "extra bytes",
        ),
        (
            "reserved field in use",
            armor(&sign_blob(manifest.as_bytes(), "skill-manifest", b"x", b"")),
            "reserved field",
        ),
        (
            "version 2",
            armor(&[b"SSHSIG".as_slice(), &2u32.to_be_bytes()].concat()),
            "version 2",
        ),
    ];
    for (name, signature, expected) in cases {
        let error = verify_test_signed(manifest.as_bytes(), &signature)
            .expect_err(name)
            .to_string();
        assert!(error.contains(expected), "{name}: {error}");
    }
}

#[test]
fn a_flipped_signature_bit_is_refused() {
    let manifest = good_manifest();
    let mut blob = sign_blob(manifest.as_bytes(), "skill-manifest", b"", b"");
    let last = blob.len() - 1;
    blob[last] ^= 1;
    let result = verify_test_signed(manifest.as_bytes(), &armor(&blob));
    refused(result, "does not match the signed file");
}

/// If OpenSSH is installed, check that it accepts what the test signer
/// writes, so the tests above are about the real format.
#[test]
fn stock_ssh_keygen_accepts_the_test_signer() {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let temp = tempfile::tempdir().unwrap();
    let manifest = good_manifest();
    fs::write(temp.path().join("sig"), sign(manifest.as_bytes())).unwrap();
    fs::write(temp.path().join("allowed"), test_key_line()).unwrap();
    let child = Command::new("ssh-keygen")
        .args(["-Y", "verify", "-I", "tester", "-n", "skill-manifest", "-f"])
        .arg(temp.path().join("allowed"))
        .arg("-s")
        .arg(temp.path().join("sig"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let Ok(mut child) = child else {
        eprintln!("ssh-keygen is not installed; skipped");
        return;
    };
    child
        .stdin
        .take()
        .unwrap()
        .write_all(manifest.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ------------------------------------------------------------------ trust

fn signer_line(options: &str) -> String {
    let key = fixture_text("signer.pub");
    format!("{SIGNER} {options} {key}")
}

#[test]
fn a_revoked_key_verifies_nothing() {
    let dir = fixtures().join("signed-by-skill-signer");
    let signers = fixture_text("allowed_signers");
    for revoked in [
        fixture_text("signer.pub"),
        fixture_text("signer.fingerprint"),
    ] {
        let error = refused(verify_with(&dir, &signers, &revoked), "is revoked");
        assert!(matches!(error, SkillError::RevokedKey { .. }));
    }
    // Revoking other keys, of any type, changes nothing.
    let others = format!(
        "# old keys\n{}{}",
        fixture_text("other.pub"),
        fixture_text("rsa.pub")
    );
    verify_with(&dir, &signers, &others).unwrap();
}

#[test]
fn a_key_trusted_for_other_namespaces_is_refused() {
    let dir = fixtures().join("signed-by-skill-signer");
    let error = refused(
        verify_with(&dir, &signer_line("namespaces=\"git,file\""), ""),
        "not trusted for namespace \"skill-manifest\"",
    );
    assert!(matches!(error, SkillError::SignerNotValid { .. }));
    verify_with(&dir, &signer_line("namespaces=\"git,skill-manifest\""), "").unwrap();
    // No namespaces option: any namespace.
    verify_with(&dir, &signer_line(""), "").unwrap();
}

#[test]
fn validity_dates_are_checked() {
    let dir = fixtures().join("signed-by-skill-signer");
    // `now()` is 2026-11-27T00:00:00Z.
    verify_with(
        &dir,
        &signer_line("valid-after=\"20261126Z\",valid-before=\"20261128Z\""),
        "",
    )
    .unwrap();
    verify_with(&dir, &signer_line("valid-after=\"20261127Z\""), "").unwrap();
    refused(
        verify_with(&dir, &signer_line("valid-after=\"202611270001Z\""), ""),
        "not valid yet",
    );
    refused(
        verify_with(&dir, &signer_line("valid-before=\"20261126Z\""), ""),
        "no longer valid",
    );
}

#[test]
fn the_first_line_that_allows_the_signature_names_the_signer() {
    let dir = fixtures().join("signed-by-skill-signer");
    let key = fixture_text("signer.pub");
    let signers = format!(
        "expired valid-before=\"20200101Z\" {key}\
         other-use namespaces=\"git\" {key}\
         current {key}\
         later {key}"
    );
    assert_eq!(verify_with(&dir, &signers, "").unwrap().signer, "current");
}

#[test]
fn trust_file_lines_outside_the_subset_are_errors() {
    let key = fixture_text("signer.pub");
    let key = key.trim();
    let rsa = fixture_text("rsa.pub");
    let cases = [
        (
            format!("ca@example.com cert-authority {key}"),
            "certificate authorities",
        ),
        (format!("someone@example.com {rsa}"), "ssh-rsa"),
        (format!("*@example.com {key}"), "one plain name per line"),
        (
            format!("a@example.com,b@example.com {key}"),
            "one plain name per line",
        ),
        (
            format!("a@example.com namespaces=\"skill-*\" {key}"),
            "exact names",
        ),
        (
            format!("a@example.com verify-required {key}"),
            "is not supported",
        ),
        (
            format!("a@example.com valid-after=\"20260101\" {key}"),
            "trailing Z",
        ),
        (
            format!("a@example.com valid-before=\"20261350Z\" {key}"),
            "YYYYMMDD",
        ),
        (format!("a@example.com namespaces=\"x {key}"), "not closed"),
        ("a@example.com ssh-ed25519".to_owned(), "expected a key"),
        ("a@example.com ssh-ed25519 !!!".to_owned(), "not base64"),
        ("a@example.com".to_owned(), "expected a key type"),
    ];
    for (line, expected) in cases {
        let text = format!("# comment\n\n{line}\n");
        let error = TrustedSigners::parse(&text).expect_err(&line);
        assert!(
            matches!(error, SkillError::TrustFile { line: 3, .. }),
            "{error}"
        );
        assert!(error.to_string().contains(expected), "{line}: {error}");
    }
}

#[test]
fn comments_and_blank_lines_in_trust_files_are_skipped() {
    let text = format!(
        "# trusted signers\n\n  # REVOKED: old@example.com ssh-ed25519 AAAA\n{}",
        fixture_text("allowed_signers")
    );
    assert!(!TrustedSigners::parse(&text).unwrap().is_empty());
    assert!(TrustedSigners::parse("# nobody\n").unwrap().is_empty());
}

#[test]
fn malformed_revoked_key_files_are_errors() {
    for text in [
        "SHA256:short",
        "fixture-signer@example.com",
        "ssh-ed25519 !!!",
        "ssh-ed25519",
    ] {
        let error = RevokedKeys::parse(text).expect_err(text);
        assert!(
            matches!(error, SkillError::TrustFile { line: 1, .. }),
            "{error}"
        );
    }
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("krl");
    fs::write(&path, b"SSHKRL\n\0\0\0\0\x01").unwrap();
    let error = RevokedKeys::load(&path).unwrap_err();
    assert!(
        error.to_string().contains("binary key revocation list"),
        "{error}"
    );
}

// ------------------------------------------- manifests that break a rule

#[test]
fn validly_signed_manifests_that_break_a_rule_are_refused() {
    let hash = "ab".repeat(32);
    let entry = format!(r#"{{"sha256": "{hash}", "size": 5}}"#);
    let files = format!(r#""files": {{"SKILL.md": {entry}"#);
    let cases: Vec<(&str, String, &str)> = vec![
        (
            r#""manifest_version": 2"#,
            r#""manifest_version": 1"#.into(),
            "manifest_version is 1",
        ),
        (
            r#""manifest_version": 2"#,
            r#""manifest_version": 3"#.into(),
            "manifest_version is 3",
        ),
        (
            r#""manifest_version": 2,"#,
            String::new(),
            "manifest_version is missing",
        ),
        (
            r#""manifest_version": 2"#,
            r#""manifest_version": "2""#.into(),
            "not valid",
        ),
        (
            r#""manifest_version": 2"#,
            r#""manifest_version": true"#.into(),
            "not valid",
        ),
        (
            r#""manifest_version": 2"#,
            r#""manifest_version": 2.0"#.into(),
            "not valid",
        ),
        (
            r#""name": "demo""#,
            r#""name": "demo", "author": "mallory""#.into(),
            "unknown field `author`",
        ),
        (
            r#""name": "demo""#,
            r#""name": "demo", "signer": {}"#.into(),
            "unknown field `signer`",
        ),
        (
            r#""name": "demo""#,
            r#""name": "demo", "name": "other""#.into(),
            "duplicate field `name`",
        ),
        (r#""name": "demo","#, String::new(), "missing field `name`"),
        (
            r#""name": "demo""#,
            r#""name": "Demo""#.into(),
            "name \"Demo\"",
        ),
        (
            r#""name": "demo""#,
            r#""name": "9lives""#.into(),
            "name \"9lives\"",
        ),
        (
            r#""name": "demo""#,
            r#""name": "has space""#.into(),
            "name \"has space\"",
        ),
        (r#""name": "demo""#, r#""name": """#.into(), "name \"\""),
        (
            r#""name": "demo""#,
            format!(r#""name": "{}""#, "a".repeat(65)),
            "is not 1-64",
        ),
        (
            r#""version": "1""#,
            r#""version": """#.into(),
            "version is empty",
        ),
        (r#""version": "1""#, r#""version": 1"#.into(), "not valid"),
        (
            r#""description": "A demonstration.""#,
            r#""description": "  ""#.into(),
            "description is empty",
        ),
        (
            r#""description": "A demonstration.""#,
            format!(r#""description": "{}""#, "é".repeat(501)),
            "longer than 500",
        ),
        (
            &files,
            format!(r#""files": {{"../SKILL.md": {entry}"#),
            "'..' component",
        ),
        (
            &files,
            format!(r#""files": {{"/etc/passwd": {entry}"#),
            "must be relative",
        ),
        (&files, format!(r#""files": {{"a//b": {entry}"#), "no empty"),
        (
            &files,
            format!(r#""files": {{"./SKILL.md": {entry}"#),
            "'.' or '..'",
        ),
        (
            &files,
            format!(r#""files": {{"a\\b": {entry}"#),
            "must use '/'",
        ),
        (
            &files,
            format!(r#""files": {{"": {entry}"#),
            "is empty or longer",
        ),
        (
            &files,
            format!(r#""files": {{"{}": {entry}"#, "p".repeat(256)),
            "longer than 255",
        ),
        (
            &files,
            format!(r#""files": {{"MANIFEST.json": {entry}"#),
            "must not list itself",
        ),
        (
            &files,
            format!(r#""files": {{"MANIFEST.json.sig": {entry}"#),
            "must not list itself",
        ),
        (
            &files,
            format!(r#"{files}, "SKILL.md": {entry}"#),
            "listed twice",
        ),
        (
            &files,
            format!(r#"{files}, "skill.MD": {entry}"#),
            "only by letter case",
        ),
        (r#""size": 5"#, r#""size": -1"#.into(), "not valid"),
        (r#""size": 5"#, r#""size": 5.5"#.into(), "not valid"),
        (
            r#""size": 5"#,
            r#""size": 67108865"#.into(),
            "at most 64 MiB",
        ),
        (
            r#""size": 5"#,
            r#""size": 5, "mode": "0755""#.into(),
            "unknown field `mode`",
        ),
        (
            r#""size": 5"#,
            r#""size": 5, "size": 6"#.into(),
            "duplicate field `size`",
        ),
        (&hash, "AB".repeat(32), "invalid sha256"),
        (&hash, "ab".repeat(31), "invalid sha256"),
        (
            r#""scripts": []"#,
            r#""scripts": ["run.sh"]"#.into(),
            "script \"run.sh\" is not in files",
        ),
        (
            r#""scripts": []"#,
            r#""scripts": ["SKILL.md", "SKILL.md"]"#.into(),
            "listed twice",
        ),
        (
            r#""network": "none""#,
            r#""network": "everything""#.into(),
            "unknown variant",
        ),
        (
            r#""network": "none""#,
            r#""network": "none", "hosts": []"#.into(),
            "unknown field `hosts`",
        ),
        (
            r#""secrets": []"#,
            r#""secrets": [""]"#.into(),
            "empty entry",
        ),
        (
            r#""secrets": []"#,
            r#""secrets": "TOKEN""#.into(),
            "not valid",
        ),
    ];
    for (from, to, expected) in cases {
        let error = verify_edited(from, &to).expect_err(&to);
        assert!(matches!(error, SkillError::Manifest(_)), "{to}: {error}");
        assert!(error.to_string().contains(expected), "{to}: {error}");
    }
}

#[test]
fn a_manifest_that_is_not_an_object_is_refused() {
    for manifest in ["", "[]", "null", "{", "\u{feff}{}", "{} {}"] {
        let result = verify_test_signed(manifest.as_bytes(), &sign(manifest.as_bytes()));
        let error = result.expect_err(manifest);
        assert!(
            matches!(error, SkillError::Manifest(_)),
            "{manifest:?}: {error}"
        );
    }
}

#[test]
fn too_many_files_are_refused() {
    let entry = format!(r#"{{"sha256": "{}", "size": 0}}"#, "ab".repeat(32));
    let files: Vec<String> = (0..513).map(|i| format!(r#""f{i}": {entry}"#)).collect();
    let result = verify_edited(
        &format!(
            r#""SKILL.md": {{"sha256": "{}", "size": 5}}"#,
            "ab".repeat(32)
        ),
        &files.join(","),
    );
    refused(result, "at most 512 files");
}

#[test]
fn optional_fields_may_be_absent() {
    let manifest = format!(
        r#"{{"manifest_version":2,"name":"a","version":"1","description":"d","files":{{"x":{{"sha256":"{}","size":0}}}}}}"#,
        "ab".repeat(32)
    );
    let verified = verify_test_signed(manifest.as_bytes(), &sign(manifest.as_bytes())).unwrap();
    assert!(verified.manifest.scripts.is_empty());
    assert_eq!(verified.manifest.requires.network, SkillNetwork::None);
}

#[test]
fn an_untrusted_manifest_is_never_parsed() {
    // Signed by a key nobody trusts, and not even JSON: the answer is about
    // the key, because the bytes are not parsed before the signature is
    // accepted.
    let manifest = b"not json";
    let error = verify_bytes(manifest, &sign(manifest)).unwrap_err();
    assert!(matches!(error, SkillError::UntrustedKey { .. }), "{error}");
}

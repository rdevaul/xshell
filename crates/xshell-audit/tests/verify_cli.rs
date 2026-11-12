//! `xshell-audit-verify` against real logs: fixtures written by the format 1,
//! 2 and 9 writers (F14), and logs written by this build.
//!
//! The fixtures were produced by the writer at commits 84917dc (format 1),
//! 07ed95e (format 2) and 2d6b8fc (format 9) with a fixed test-only signing
//! key of 32 bytes of 0x14; only its public half is committed. The format 9
//! log holds a tool argument of 1.0715660391465826e-75, which the format 9
//! verifier rejected as a hash mismatch.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;
use xshell_audit::{
    AUDIT_FORMAT_VERSION, AuditEvent, AuditLogWriter, HashingScheme, SigningIdentity,
    VerifyOptions, session_id_from_path, verify_log, verify_log_with,
};

fn fixture(format: u32) -> (PathBuf, PathBuf) {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("format{format}"));
    let log = fs::read_dir(directory.join("sessions"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .unwrap();
    (directory, log)
}

fn verify(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xshell-audit-verify"))
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn f14_legacy_fixtures_verify() {
    for (format, records) in [(1, 9), (2, 10), (9, 12)] {
        let (directory, log) = fixture(format);
        let public_key = directory.join("signing-key.pub");
        let options = VerifyOptions {
            session_id: session_id_from_path(&log),
            index: Some(directory.join("checkpoints.jsonl")),
        };
        let report = verify_log_with(&log, &public_key, &options)
            .unwrap_or_else(|error| panic!("format {format} fixture: {error:#}"));
        assert_eq!(report.format_version, format);
        assert_eq!(report.hashing, HashingScheme::LegacyReencoded);
        assert_eq!(report.records, records, "format {format}");
        assert!(report.final_checkpoint, "format {format}");
        assert_eq!(report.unsigned_records, 0);
        assert!(report.index_checked);

        let contents = fs::read_to_string(&log).unwrap();
        if format <= 2 {
            assert!(contents.contains(r#""type":"logical_session_attached""#));
            assert!(!contents.contains("terminal_stream"));
        } else {
            assert!(contents.contains(r#""terminal_stream":null"#));
            assert!(contents.contains("1.0715660391465826e-75"));
        }

        let output = verify(&[
            "--public-key".as_ref(),
            public_key.as_os_str(),
            "--index".as_ref(),
            directory.join("checkpoints.jsonl").as_os_str(),
            "--require-final".as_ref(),
            log.as_os_str(),
        ]);
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(stdout(&output).contains("hashing: legacy-reencoded"));
        assert!(stdout(&output).contains("checkpoint index: matches"));
    }
}

#[test]
fn f14_legacy_fixtures_still_detect_edits() {
    let temp = TempDir::new().unwrap();
    for format in [1, 2, 9] {
        let (directory, log) = fixture(format);
        let public_key = directory.join("signing-key.pub");
        let copy = temp.path().join(log.file_name().unwrap());
        let contents = fs::read_to_string(&log).unwrap();
        fs::write(
            &copy,
            contents.replacen(r#""user":"op""#, r#""user":"root""#, 1),
        )
        .unwrap();
        assert!(verify_log(&copy, &public_key).is_err(), "format {format}");
    }
}

struct Daemon {
    _temp: TempDir,
    directory: PathBuf,
}

impl Daemon {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let directory = temp.path().join("audit");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            _temp: temp,
            directory,
        }
    }

    fn writer(&self, interval: u64) -> AuditLogWriter {
        let identity = SigningIdentity::load_or_create(&self.directory).unwrap();
        AuditLogWriter::create(&self.directory, identity, 501, interval).unwrap()
    }

    fn public_key(&self) -> PathBuf {
        self.directory.join("signing-key.pub")
    }

    fn index(&self) -> PathBuf {
        self.directory.join("checkpoints.jsonl")
    }
}

fn input(text: &str) -> AuditEvent {
    AuditEvent::Input {
        route: "agent".into(),
        text: text.into(),
        images: Vec::new(),
    }
}

#[test]
fn f14_the_cli_binds_a_log_to_its_file_name() {
    let daemon = Daemon::new();
    let mut first = daemon.writer(16);
    let first_path = first.path().to_owned();
    let first_id = first.session_id().to_owned();
    first.append(input("first")).unwrap();
    first.close().unwrap();
    let mut second = daemon.writer(16);
    let second_path = second.path().to_owned();
    let second_id = second.session_id().to_owned();
    second.append(input("second")).unwrap();
    second.close().unwrap();
    let key = daemon.public_key();

    let output = verify(&[
        "--public-key".as_ref(),
        key.as_os_str(),
        first_path.as_os_str(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("hashing: stored-bytes"));
    assert!(stdout(&output).contains(&format!("format: {AUDIT_FORMAT_VERSION}")));

    // Swap the second session's closed log in under the first one's name.
    fs::copy(&second_path, &first_path).unwrap();
    let output = verify(&[
        "--public-key".as_ref(),
        key.as_os_str(),
        first_path.as_os_str(),
    ]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("not the expected"),
        "{}",
        stderr(&output)
    );

    // An explicit ID replaces the file name.
    let output = verify(&[
        "--public-key".as_ref(),
        key.as_os_str(),
        "--session-id".as_ref(),
        second_id.as_ref(),
        first_path.as_os_str(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let output = verify(&[
        "--public-key".as_ref(),
        key.as_os_str(),
        "--session-id".as_ref(),
        first_id.as_ref(),
        first_path.as_os_str(),
    ]);
    assert!(!output.status.success());
}

#[test]
fn f14_the_cli_detects_a_rollback_through_the_index() {
    let daemon = Daemon::new();
    let mut writer = daemon.writer(1);
    let path = writer.path().to_owned();
    writer.append(input("one")).unwrap();
    writer.append(input("two")).unwrap();
    writer.close().unwrap();
    let key = daemon.public_key();
    let index = daemon.index();
    let args = |path: &Path| -> Output {
        verify(&[
            "--public-key".as_ref(),
            key.as_os_str(),
            "--index".as_ref(),
            index.as_os_str(),
            "--allow-open".as_ref(),
            path.as_os_str(),
        ])
    };
    let output = args(&path);
    assert!(output.status.success(), "{}", stderr(&output));

    let contents = fs::read_to_string(&path).unwrap();
    let rolled_back: Vec<&str> = contents.lines().take(4).collect();
    fs::write(&path, rolled_back.join("\n") + "\n").unwrap();
    assert!(verify_log(&path, &key).is_ok());
    let output = args(&path);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("truncated or rolled back"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn f14_require_final_and_the_unsigned_tail_warning() {
    let daemon = Daemon::new();
    let mut writer = daemon.writer(2);
    let path = writer.path().to_owned();
    for text in ["one", "two", "three"] {
        writer.append(input(text)).unwrap();
    }
    let key = daemon.public_key();
    let run = |flag: &str| {
        verify(&[
            "--public-key".as_ref(),
            key.as_os_str(),
            flag.as_ref(),
            path.as_os_str(),
        ])
    };

    // Open: one record after the last checkpoint.
    let output = run("--require-final");
    assert!(!output.status.success());
    assert!(stdout(&output).contains("unsigned records: 1"));
    assert!(stderr(&output).contains("warning: 1 record(s) after the last signed checkpoint"));
    assert!(stderr(&output).contains("no final signed checkpoint"));
    let output = run("--allow-open");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("warning: 1 record(s)"));

    // Closed: everything signed, no warning, and nothing may follow.
    writer.close().unwrap();
    let output = run("--require-final");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("unsigned records: 0"));
    assert!(!stderr(&output).contains("record(s) after"));

    let contents = fs::read_to_string(&path).unwrap();
    let first_record = contents.lines().next().unwrap();
    fs::write(&path, format!("{contents}{first_record}\n")).unwrap();
    let output = run("--allow-open");
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("after the final checkpoint"),
        "{}",
        stderr(&output)
    );
}

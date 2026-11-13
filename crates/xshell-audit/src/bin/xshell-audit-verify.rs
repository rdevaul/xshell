use anyhow::{Context, Result, bail};
use clap::Parser;
use std::path::PathBuf;
use xshell_audit::{VerifyOptions, session_id_from_path, verify_log_with};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Verify an xshell audit log and its signed checkpoints"
)]
struct Args {
    log: PathBuf,

    #[arg(long)]
    public_key: Option<PathBuf>,

    /// The audit session the log must hold. Defaults to the log's file name
    /// (`sessions/<session-id>.jsonl`).
    #[arg(long, value_name = "ID")]
    session_id: Option<String>,

    /// Check the log's last checkpoint against the latest one recorded for
    /// its session in this checkpoint index (the daemon's
    /// `checkpoints.jsonl`), to detect a rolled-back or replaced log.
    #[arg(long, value_name = "PATH")]
    index: Option<PathBuf>,

    /// Fail unless the last entry is a final checkpoint covering every
    /// record. This is the default; the flag states it explicitly.
    #[arg(long, conflicts_with = "allow_open")]
    require_final: bool,

    /// Accept a log without a final checkpoint, such as one still being
    /// written. Records after its last checkpoint are reported as unsigned.
    #[arg(long)]
    allow_open: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let using_adjacent_key = args.public_key.is_none();
    let public_key = args.public_key.unwrap_or_else(|| {
        args.log
            .parent()
            .and_then(|sessions| sessions.parent())
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("signing-key.pub")
    });
    if using_adjacent_key {
        eprintln!(
            "warning: using the public key beside the log; authenticity requires a separately trusted copy"
        );
    }
    let session_id = match args.session_id {
        Some(id) => id,
        None => session_id_from_path(&args.log).with_context(|| {
            format!(
                "cannot take a session ID from the name of {}; pass --session-id",
                args.log.display()
            )
        })?,
    };
    let options = VerifyOptions {
        session_id: Some(session_id),
        index: args.index,
    };
    let report = verify_log_with(&args.log, &public_key, &options)
        .with_context(|| format!("verification failed for {}", args.log.display()))?;
    println!("session: {}", report.session_id);
    println!("format: {}", report.format_version);
    println!("hashing: {}", report.hashing);
    println!("records: {}", report.records);
    println!("checkpoints: {}", report.checkpoints);
    println!("signed records: {}", report.signed_records);
    println!("unsigned records: {}", report.unsigned_records);
    println!("chain head: {}", report.chain_head);
    println!(
        "final checkpoint: {}",
        if report.final_checkpoint {
            "present"
        } else {
            "MISSING"
        }
    );
    println!(
        "checkpoint index: {}",
        if report.index_checked {
            "matches"
        } else {
            "not checked"
        }
    );
    if report.unsigned_records > 0 {
        eprintln!(
            "warning: {} record(s) after the last signed checkpoint are covered only by the \
             unkeyed hash chain; anyone able to write the log could have altered or added them",
            report.unsigned_records
        );
    }
    if !report.final_checkpoint && (args.require_final || !args.allow_open) {
        bail!("log has no final signed checkpoint; it may be incomplete or truncated");
    }
    Ok(())
}

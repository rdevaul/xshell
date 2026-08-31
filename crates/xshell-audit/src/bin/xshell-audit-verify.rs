use anyhow::{Context, Result, bail};
use clap::Parser;
use std::path::PathBuf;
use xshell_audit::verify_log;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Verify an xshell audit log and its signed checkpoints"
)]
struct Args {
    log: PathBuf,

    #[arg(long)]
    public_key: Option<PathBuf>,
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
    let report = verify_log(&args.log, &public_key)
        .with_context(|| format!("verification failed for {}", args.log.display()))?;
    println!("session: {}", report.session_id);
    println!("records: {}", report.records);
    println!("checkpoints: {}", report.checkpoints);
    println!("chain head: {}", report.chain_head);
    println!(
        "final checkpoint: {}",
        if report.final_checkpoint {
            "present"
        } else {
            "MISSING"
        }
    );
    if !report.final_checkpoint {
        bail!("log has no final signed checkpoint; it may be incomplete or truncated");
    }
    Ok(())
}

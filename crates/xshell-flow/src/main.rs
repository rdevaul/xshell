use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use xshell_flow::Flow;

#[derive(Debug, Parser)]
#[command(
    name = "xshell-flow",
    about = "Inspect the FutureShell Flow IR prototype"
)]
struct Arguments {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Parse and validate a Flow IR JSON document.
    Check { file: PathBuf },
    /// Print the semantic hash of a valid Flow IR document.
    Hash { file: PathBuf },
    /// Print normalized semantic JSON, without editor annotations.
    Normalize { file: PathBuf },
    /// Render the flow as deterministic Graphviz DOT.
    Dot { file: PathBuf },
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    match arguments.command {
        Command::Check { file } => {
            let flow = read_flow(&file)?;
            check(&flow)?;
            println!("{}: valid {}", file.display(), flow.schema);
        }
        Command::Hash { file } => {
            let flow = read_flow(&file)?;
            check(&flow)?;
            println!("{}", flow.semantic_hash()?);
        }
        Command::Normalize { file } => {
            let flow = read_flow(&file)?;
            check(&flow)?;
            let normalized = xshell_flow::canonical_semantic_bytes(&flow)?;
            println!("{}", String::from_utf8(normalized).expect("JSON is UTF-8"));
        }
        Command::Dot { file } => {
            let flow = read_flow(&file)?;
            check(&flow)?;
            print!("{}", xshell_flow::to_dot(&flow));
        }
    }
    Ok(())
}

fn read_flow(path: &PathBuf) -> Result<Flow> {
    let source =
        std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    serde_json::from_slice(&source).with_context(|| format!("could not parse {}", path.display()))
}

fn check(flow: &Flow) -> Result<()> {
    if let Err(error) = flow.validate() {
        bail!(error);
    }
    Ok(())
}

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use xshell_flow::Flow;

#[derive(Debug, Parser)]
#[command(
    name = "xshell-plan",
    about = "Lower FutureShell Flow IR into an immutable Plan V0"
)]
struct Arguments {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Lower a flow and print its plan.
    Build {
        file: PathBuf,
        /// Emit the complete plan artifact as JSON instead of a summary.
        #[arg(long)]
        json: bool,
        /// Write JSON to a file. Requires --json.
        #[arg(long, value_name = "PATH", requires = "json")]
        output: Option<PathBuf>,
    },
    /// Print only the plan's semantic hash.
    Hash { file: PathBuf },
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    match arguments.command {
        Command::Build { file, json, output } => {
            let plan = build(&file)?;
            if json {
                let mut encoded = serde_json::to_vec_pretty(&plan.artifact()?)?;
                encoded.push(b'\n');
                if let Some(output) = output {
                    std::fs::write(&output, encoded)
                        .with_context(|| format!("could not write {}", output.display()))?;
                } else {
                    print!("{}", String::from_utf8(encoded).expect("JSON is UTF-8"));
                }
            } else {
                print_summary(&plan)?;
            }
        }
        Command::Hash { file } => {
            println!("{}", build(&file)?.hash()?);
        }
    }
    Ok(())
}

fn build(path: &Path) -> Result<xshell_plan::Plan> {
    let source =
        std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    let flow: Flow = serde_json::from_slice(&source)
        .with_context(|| format!("could not parse {}", path.display()))?;
    match xshell_plan::lower(&flow) {
        Ok(plan) => Ok(plan),
        Err(error) => bail!(error),
    }
}

fn print_summary(plan: &xshell_plan::Plan) -> Result<()> {
    println!("Plan: {}", plan.name);
    println!("Schema: {}", plan.schema);
    println!("Hash: {}", plan.hash()?);
    println!("Tasks: {}", plan.tasks.len());
    println!("Loops: {}", plan.loops.len());
    println!(
        "Transactions: {}",
        plan.regions
            .iter()
            .filter(|region| {
                matches!(
                    &region.kind,
                    xshell_plan::PlanRegionKind::Transaction { .. }
                )
            })
            .count()
    );
    println!("Fully resolved: {}", plan.resolution.fully_resolved);
    if !plan.resolution.blockers.is_empty() {
        println!("Resolution blockers:");
        for blocker in &plan.resolution.blockers {
            match blocker {
                xshell_plan::ResolutionBlocker::UnresolvedProgram { task, source } => {
                    println!("  - task {task}: unresolved FutureShell program {source}");
                }
                xshell_plan::ResolutionBlocker::UncheckedPredicate {
                    contract,
                    predicate,
                } => {
                    println!("  - contract {contract}: unchecked predicate signature {predicate}");
                }
            }
        }
    }
    Ok(())
}

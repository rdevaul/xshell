use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
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
        #[command(flatten)]
        resolution: ResolutionFiles,
        /// Emit the complete plan artifact as JSON instead of a summary.
        #[arg(long)]
        json: bool,
        /// Write JSON to a file. Requires --json.
        #[arg(long, value_name = "PATH", requires = "json")]
        output: Option<PathBuf>,
    },
    /// Print only the plan's semantic hash.
    Hash {
        file: PathBuf,
        #[command(flatten)]
        resolution: ResolutionFiles,
    },
}

#[derive(Debug, Args)]
struct ResolutionFiles {
    /// Resolve program references from this provisional catalog.
    #[arg(long, value_name = "PATH")]
    program_catalog: Option<PathBuf>,
    /// Check contract predicates against this provisional catalog.
    #[arg(long, value_name = "PATH")]
    predicate_catalog: Option<PathBuf>,
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    match arguments.command {
        Command::Build {
            file,
            resolution,
            json,
            output,
        } => {
            let plan = build(&file, &resolution)?;
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
        Command::Hash { file, resolution } => {
            println!("{}", build(&file, &resolution)?.hash()?);
        }
    }
    Ok(())
}

fn build(path: &Path, resolution: &ResolutionFiles) -> Result<xshell_plan::Plan> {
    let source =
        std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    let flow: Flow = serde_json::from_slice(&source)
        .with_context(|| format!("could not parse {}", path.display()))?;
    let plan = match xshell_plan::lower(&flow) {
        Ok(plan) => plan,
        Err(error) => bail!(error),
    };
    if resolution.program_catalog.is_none() && resolution.predicate_catalog.is_none() {
        return Ok(plan);
    }
    let programs = match &resolution.program_catalog {
        Some(path) => read_json(path, "program catalog")?,
        None => xshell_plan::ProgramCatalog::default(),
    };
    let predicates = match &resolution.predicate_catalog {
        Some(path) => read_json(path, "predicate catalog")?,
        None => xshell_plan::PredicateCatalog::default(),
    };
    match xshell_plan::resolve(&plan, &programs, &predicates) {
        Ok(plan) => Ok(plan),
        Err(error) => bail!(error),
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path, description: &str) -> Result<T> {
    let source = std::fs::read(path)
        .with_context(|| format!("could not read {description} {}", path.display()))?;
    serde_json::from_slice(&source)
        .with_context(|| format!("could not parse {description} {}", path.display()))
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
    let resolved_programs = plan
        .tasks
        .iter()
        .filter(|task| {
            matches!(
                task.operation,
                xshell_plan::TaskOperation::Program {
                    resolution: Some(_),
                    ..
                }
            )
        })
        .count();
    let total_programs = plan
        .tasks
        .iter()
        .filter(|task| matches!(task.operation, xshell_plan::TaskOperation::Program { .. }))
        .count();
    let (resolved_predicates, total_predicates) = plan
        .contracts
        .iter()
        .map(|contract| predicate_counts(&contract.expression))
        .fold((0, 0), |left, right| (left.0 + right.0, left.1 + right.1));
    println!("Resolved programs: {resolved_programs}/{total_programs}");
    println!("Resolved predicates: {resolved_predicates}/{total_predicates}");
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

fn predicate_counts(expression: &xshell_plan::PlanContractExpression) -> (usize, usize) {
    match expression {
        xshell_plan::PlanContractExpression::Predicate { resolution, .. } => {
            (usize::from(resolution.is_some()), 1)
        }
        xshell_plan::PlanContractExpression::All { clauses }
        | xshell_plan::PlanContractExpression::Any { clauses } => clauses
            .iter()
            .map(predicate_counts)
            .fold((0, 0), |left, right| (left.0 + right.0, left.1 + right.1)),
        xshell_plan::PlanContractExpression::Not { clause } => predicate_counts(clause),
    }
}

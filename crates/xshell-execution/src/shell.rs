//! How an agent's shell commands are started.
//!
//! `run_shell` builds a command line and supervises the process: its process
//! group, bounded output, time limit and cancellation. A [`ShellRunner`]
//! sits between the two and decides what the command line is. The ambient
//! runner runs the shell as it is. A containing runner wraps the same shell
//! in whatever enforces the session's containment, such as a sandbox
//! profile, and everything after spawning stays the same.
//!
//! See `docs/shell-sandbox.md`.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::process::Command;

/// How strongly a runner contains the commands it starts. The names are
/// FutureShell's assurance levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellAssurance {
    /// The command has the full authority of the account it runs under.
    Ambient,
    /// The platform confines the command's file access to declared roots
    /// and applies its network setting.
    Isolated,
    /// The command runs in a container or virtual machine.
    External,
}

impl ShellAssurance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ambient => "ambient",
            Self::Isolated => "isolated",
            Self::External => "external",
        }
    }
}

/// Starts shell commands for one session.
pub trait ShellRunner: Send + Sync + std::fmt::Debug {
    /// What this runner enforces.
    fn assurance(&self) -> ShellAssurance;

    /// The command line that runs `command` with `shell -c` in `cwd` under
    /// this runner. The caller sets the environment, standard streams and
    /// process group, so those apply to whatever is returned.
    fn command(&self, shell: &Path, command: &str, cwd: &Path) -> Result<Command>;
}

/// Runs the shell directly, with nothing between it and the system.
#[derive(Debug, Clone, Copy, Default)]
pub struct AmbientRunner;

impl ShellRunner for AmbientRunner {
    fn assurance(&self) -> ShellAssurance {
        ShellAssurance::Ambient
    }

    fn command(&self, shell: &Path, command: &str, cwd: &Path) -> Result<Command> {
        let mut process = Command::new(shell);
        process.arg("-c").arg(command).current_dir(cwd);
        Ok(process)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_order_from_least_to_most_contained() {
        assert!(ShellAssurance::Ambient < ShellAssurance::Isolated);
        assert!(ShellAssurance::Isolated < ShellAssurance::External);
        assert_eq!(
            serde_json::to_string(&ShellAssurance::Isolated).unwrap(),
            "\"isolated\""
        );
        assert_eq!(ShellAssurance::External.as_str(), "external");
    }

    #[test]
    fn the_ambient_runner_runs_the_shell_as_it_is() {
        let command = AmbientRunner
            .command(Path::new("/bin/sh"), "echo hi", Path::new("/tmp"))
            .unwrap();
        let command = command.as_std();
        assert_eq!(command.get_program(), "/bin/sh");
        let arguments: Vec<_> = command.get_args().collect();
        assert_eq!(arguments, ["-c", "echo hi"]);
        assert_eq!(command.get_current_dir(), Some(Path::new("/tmp")));
        assert_eq!(AmbientRunner.assurance(), ShellAssurance::Ambient);
    }
}

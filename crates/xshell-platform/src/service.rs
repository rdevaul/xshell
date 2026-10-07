//! Per-user service management for `xshelld`.
//!
//! The session fabric assumes a daemon is already running on every host it
//! talks to. Installing and starting that daemon is the one step a user cannot
//! perform through xshell itself, and it is also the step a remote bootstrap
//! has to drive on a machine nobody is sitting at. This module turns that into
//! a single platform-neutral surface: `launchd` user agents on macOS,
//! `systemd --user` units on Linux.
//!
//! Everything that decides *what* to do — the unit text, the argument vectors,
//! and the interpretation of a service manager's output — is a pure function so
//! it can be asserted in CI on a machine where neither `launchctl` nor
//! `systemctl` can usefully run. [`ServiceManager::run`] is the only part that
//! touches a process.
//!
//! Scope is deliberately per-user. These services run as the invoking user,
//! with no root, no system-wide unit, and no inbound network port, which is the
//! same trust boundary the daemon's own socket checks assume.

use anyhow::{Context, Result, bail};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Reverse-DNS label for the macOS launch agent.
pub const LAUNCHD_LABEL: &str = "com.xshell.xshelld";
/// Unit name for the Linux user service.
pub const SYSTEMD_UNIT: &str = "xshelld.service";

/// The platform service manager that owns a per-user `xshelld`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceManager {
    /// macOS launch agent in the calling user's GUI domain.
    Launchd,
    /// Linux `systemd --user` unit.
    Systemd,
}

/// What a service verb should do to the installed unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceVerb {
    Start,
    Stop,
    Restart,
}

impl fmt::Display for ServiceVerb {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        })
    }
}

/// Everything needed to write a unit that starts the daemon the same way the
/// user would.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceDefinition {
    /// Absolute path to the `xshelld` binary the unit should run.
    pub program: PathBuf,
    /// Configuration file to pin with `--config`.
    ///
    /// A service does not inherit the shell environment that normally supplies
    /// `XSHELL_CONFIG`, so leaving this empty would silently start the daemon
    /// against different configuration than the user's own invocations. The
    /// installer resolves the active path and pins it here.
    pub config: Option<PathBuf>,
    /// Directory for `launchd` stdout/stderr capture. Unused under systemd,
    /// which routes the same output to the journal.
    pub log_directory: PathBuf,
}

impl ServiceDefinition {
    /// Argument vector the unit invokes, as the user would type it.
    #[must_use]
    pub fn program_arguments(&self) -> Vec<String> {
        let mut arguments = vec![self.program.display().to_string()];
        if let Some(config) = &self.config {
            arguments.push("--config".to_owned());
            arguments.push(config.display().to_string());
        }
        arguments
    }
}

/// One command to hand to the platform service manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub arguments: Vec<String>,
    /// A command whose failure is expected and not an error — tearing down a
    /// unit that was never loaded, for instance.
    pub tolerate_failure: bool,
}

impl CommandSpec {
    fn new(program: &str, arguments: &[String]) -> Self {
        Self {
            program: program.to_owned(),
            arguments: arguments.to_vec(),
            tolerate_failure: false,
        }
    }

    fn tolerant(program: &str, arguments: &[String]) -> Self {
        Self {
            tolerate_failure: true,
            ..Self::new(program, arguments)
        }
    }

    /// Render as a copy-pasteable shell command for diagnostics.
    #[must_use]
    pub fn display(&self) -> String {
        std::iter::once(self.program.clone())
            .chain(self.arguments.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Where the installed unit lives and what has to run to activate it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub unit_path: PathBuf,
    pub unit_contents: String,
    pub activate: Vec<CommandSpec>,
}

/// Observed state of the per-user service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceState {
    /// No unit file is present.
    NotInstalled,
    /// A unit is present but the service manager does not have it loaded.
    Installed,
    /// Loaded and not currently running.
    Stopped,
    /// Mid-transition, with the service manager's own name for the phase.
    ///
    /// launchd reports its spawn helper (`xpcproxy`) here for a moment after
    /// `kickstart`, and systemd reports `activating`. Neither is an error, and
    /// neither is a settled state worth reporting to a user as final.
    Starting { detail: String },
    /// Running, with the pid when the service manager reports one.
    Running { pid: Option<u32> },
    /// Loaded but in an error state the service manager named.
    Failed { detail: String },
}

impl ServiceState {
    /// Whether this is a transient phase that is worth waiting out.
    #[must_use]
    pub fn is_settling(&self) -> bool {
        matches!(self, Self::Starting { .. })
    }
}

impl fmt::Display for ServiceState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => formatter.write_str("not installed"),
            Self::Installed => formatter.write_str("installed, not loaded"),
            Self::Stopped => formatter.write_str("loaded, stopped"),
            Self::Starting { detail } => write!(formatter, "starting ({detail})"),
            Self::Running { pid: Some(pid) } => write!(formatter, "running (pid {pid})"),
            Self::Running { pid: None } => formatter.write_str("running"),
            Self::Failed { detail } => write!(formatter, "failed: {detail}"),
        }
    }
}

impl ServiceManager {
    /// The service manager for the host this binary is running on.
    pub fn detect() -> Result<Self> {
        if cfg!(target_os = "macos") {
            Ok(Self::Launchd)
        } else if cfg!(target_os = "linux") {
            Ok(Self::Systemd)
        } else {
            bail!(
                "xshelld service management supports macOS (launchd) and Linux (systemd --user); \
                 start the daemon directly on this platform"
            )
        }
    }

    /// Human-readable name used in diagnostics.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Launchd => "launchd",
            Self::Systemd => "systemd --user",
        }
    }

    /// Path of the unit file this manager reads, under `home`.
    #[must_use]
    pub fn unit_path(&self, home: &Path) -> PathBuf {
        match self {
            Self::Launchd => home
                .join("Library/LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist")),
            Self::Systemd => home.join(".config/systemd/user").join(SYSTEMD_UNIT),
        }
    }

    /// The `launchctl` domain target for this user's agents.
    fn domain(uid: u32) -> String {
        format!("gui/{uid}")
    }

    fn service_target(uid: u32) -> String {
        format!("{}/{LAUNCHD_LABEL}", Self::domain(uid))
    }

    /// Render the unit text for `definition`.
    #[must_use]
    pub fn render_unit(&self, definition: &ServiceDefinition) -> String {
        match self {
            Self::Launchd => render_plist(definition),
            Self::Systemd => render_systemd_unit(definition),
        }
    }

    /// Everything required to install and activate the service.
    #[must_use]
    pub fn install_plan(
        &self,
        home: &Path,
        uid: u32,
        definition: &ServiceDefinition,
    ) -> InstallPlan {
        let unit_path = self.unit_path(home);
        let activate = match self {
            Self::Launchd => {
                let target = Self::service_target(uid);
                let domain = Self::domain(uid);
                vec![
                    // Tear down any previous generation first, so reinstalling
                    // over a changed binary path actually takes effect.
                    CommandSpec::tolerant("launchctl", &["bootout".into(), target.clone()]),
                    CommandSpec::new(
                        "launchctl",
                        &["bootstrap".into(), domain, unit_path.display().to_string()],
                    ),
                    CommandSpec::new("launchctl", &["kickstart".into(), target]),
                ]
            }
            Self::Systemd => vec![
                CommandSpec::new("systemctl", &["--user".into(), "daemon-reload".into()]),
                CommandSpec::new(
                    "systemctl",
                    &["--user".into(), "enable".into(), SYSTEMD_UNIT.into()],
                ),
                CommandSpec::new(
                    "systemctl",
                    &["--user".into(), "restart".into(), SYSTEMD_UNIT.into()],
                ),
            ],
        };
        InstallPlan {
            unit_path,
            unit_contents: self.render_unit(definition),
            activate,
        }
    }

    /// Commands that remove the service. The caller deletes the unit file
    /// between the teardown and reload steps.
    #[must_use]
    pub fn uninstall_commands(&self, uid: u32) -> (Vec<CommandSpec>, Vec<CommandSpec>) {
        match self {
            Self::Launchd => (
                vec![CommandSpec::tolerant(
                    "launchctl",
                    &["bootout".into(), Self::service_target(uid)],
                )],
                Vec::new(),
            ),
            Self::Systemd => (
                vec![CommandSpec::tolerant(
                    "systemctl",
                    &[
                        "--user".into(),
                        "disable".into(),
                        "--now".into(),
                        SYSTEMD_UNIT.into(),
                    ],
                )],
                vec![CommandSpec::new(
                    "systemctl",
                    &["--user".into(), "daemon-reload".into()],
                )],
            ),
        }
    }

    /// Commands implementing a lifecycle verb against an installed unit.
    #[must_use]
    pub fn verb_commands(&self, verb: ServiceVerb, uid: u32, unit_path: &Path) -> Vec<CommandSpec> {
        match self {
            Self::Launchd => {
                let target = Self::service_target(uid);
                match verb {
                    // `bootstrap` is tolerated because the agent may already be
                    // loaded; `kickstart` is what actually guarantees it runs.
                    ServiceVerb::Start => vec![
                        CommandSpec::tolerant(
                            "launchctl",
                            &[
                                "bootstrap".into(),
                                Self::domain(uid),
                                unit_path.display().to_string(),
                            ],
                        ),
                        CommandSpec::new("launchctl", &["kickstart".into(), target]),
                    ],
                    ServiceVerb::Stop => {
                        vec![CommandSpec::new("launchctl", &["bootout".into(), target])]
                    }
                    // `-k` sends SIGTERM and waits, so the daemon runs its own
                    // graceful drain before the replacement starts.
                    ServiceVerb::Restart => vec![CommandSpec::new(
                        "launchctl",
                        &["kickstart".into(), "-k".into(), target],
                    )],
                }
            }
            Self::Systemd => vec![CommandSpec::new(
                "systemctl",
                &["--user".into(), verb.to_string(), SYSTEMD_UNIT.into()],
            )],
        }
    }

    /// Command whose output [`Self::parse_state`] interprets.
    #[must_use]
    pub fn status_command(&self, uid: u32) -> CommandSpec {
        match self {
            Self::Launchd => {
                CommandSpec::tolerant("launchctl", &["print".into(), Self::service_target(uid)])
            }
            Self::Systemd => CommandSpec::tolerant(
                "systemctl",
                &[
                    "--user".into(),
                    "show".into(),
                    SYSTEMD_UNIT.into(),
                    "--property=ActiveState".into(),
                    "--property=MainPID".into(),
                ],
            ),
        }
    }

    /// Interpret the status command's output.
    ///
    /// `unit_present` distinguishes "never installed" from "installed but the
    /// service manager has not loaded it", which are different problems with
    /// different fixes.
    #[must_use]
    pub fn parse_state(&self, unit_present: bool, success: bool, output: &str) -> ServiceState {
        if !unit_present {
            return ServiceState::NotInstalled;
        }
        match self {
            Self::Launchd => {
                if !success {
                    // `launchctl print` fails outright when the label is not
                    // bootstrapped into the domain.
                    return ServiceState::Installed;
                }
                let pid = field_after(output, "pid = ").and_then(|value| value.parse().ok());
                match field_after(output, "state = ").as_deref() {
                    Some("running") => ServiceState::Running { pid },
                    Some("not running") => ServiceState::Stopped,
                    // `state` is a launchd-internal lifecycle label, not a
                    // documented enum. Treating an unrecognised value as a
                    // failure would report a healthy service as broken, so
                    // trust the pid when there is one and otherwise call it
                    // what it is: an unsettled transition.
                    Some(other) if pid.is_some() => {
                        let _ = other;
                        ServiceState::Running { pid }
                    }
                    Some(other) => ServiceState::Starting {
                        detail: other.to_owned(),
                    },
                    None if pid.is_some() => ServiceState::Running { pid },
                    None => ServiceState::Stopped,
                }
            }
            Self::Systemd => {
                if !success {
                    return ServiceState::Installed;
                }
                let pid = property(output, "MainPID")
                    .and_then(|value| value.parse::<u32>().ok())
                    .filter(|pid| *pid != 0);
                match property(output, "ActiveState").as_deref() {
                    Some("active") => ServiceState::Running { pid },
                    Some(phase @ ("activating" | "reloading" | "deactivating")) => {
                        ServiceState::Starting {
                            detail: phase.to_owned(),
                        }
                    }
                    Some("inactive") => ServiceState::Stopped,
                    Some("failed") => ServiceState::Failed {
                        detail: "unit reported ActiveState=failed".to_owned(),
                    },
                    Some(other) => ServiceState::Failed {
                        detail: format!("unexpected ActiveState={other}"),
                    },
                    // A unit file on disk that `show` does not recognise means
                    // the manager has not reloaded since it was written.
                    None => ServiceState::Installed,
                }
            }
        }
    }

    /// Whether a `systemd --user` service survives logout.
    ///
    /// Without lingering the user manager is torn down when the last session
    /// ends, so an SSH-installed daemon stops the moment the installing
    /// connection closes — exactly the case remote bootstrap cares about.
    /// `launchd` agents have no equivalent switch; see the module notes.
    #[must_use]
    pub fn parse_linger(output: &str) -> Option<bool> {
        property(output, "Linger").map(|value| value == "yes")
    }

    /// Command that reports whether lingering is enabled for `user`.
    #[must_use]
    pub fn linger_command(user: &str) -> CommandSpec {
        CommandSpec::tolerant(
            "loginctl",
            &[
                "show-user".into(),
                user.to_owned(),
                "--property=Linger".into(),
            ],
        )
    }

    /// Run one command, returning its stdout. Fails with the service manager's
    /// own stderr, which is far more useful than a generic exit-status message.
    pub fn run(spec: &CommandSpec) -> Result<(bool, String)> {
        let output = Command::new(&spec.program)
            .args(&spec.arguments)
            .output()
            .with_context(|| format!("cannot run `{}`", spec.display()))?;
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.status.success() {
            if !spec.tolerate_failure {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let detail = stderr.trim();
                bail!(
                    "`{}` failed{}{}",
                    spec.display(),
                    if detail.is_empty() { "" } else { ": " },
                    detail
                );
            }
            // A tolerated failure still carries diagnostics worth parsing.
            text.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        Ok((output.status.success(), text))
    }
}

/// Value following `key` on the line that contains it, trimmed.
fn field_after(output: &str, key: &str) -> Option<String> {
    output.lines().find_map(|line| {
        line.trim()
            .strip_prefix(key)
            .map(|value| value.trim().trim_end_matches(';').trim().to_owned())
    })
}

/// Value of a `Key=Value` property line.
fn property(output: &str, key: &str) -> Option<String> {
    output.lines().find_map(|line| {
        line.trim()
            .strip_prefix(key)
            .and_then(|rest| rest.strip_prefix('='))
            .map(|value| value.trim().to_owned())
    })
}

/// Escape text for an XML character-data position.
fn escape_xml(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn render_plist(definition: &ServiceDefinition) -> String {
    let arguments = definition
        .program_arguments()
        .iter()
        .map(|argument| format!("        <string>{}</string>\n", escape_xml(argument)))
        .collect::<String>();
    let out_log = definition.log_directory.join("xshelld.out.log");
    let error_log = definition.log_directory.join("xshelld.err.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{arguments}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>{out_log}</string>
    <key>StandardErrorPath</key>
    <string>{error_log}</string>
</dict>
</plist>
"#,
        label = LAUNCHD_LABEL,
        out_log = escape_xml(&out_log.display().to_string()),
        error_log = escape_xml(&error_log.display().to_string()),
    )
}

fn render_systemd_unit(definition: &ServiceDefinition) -> String {
    // systemd splits ExecStart on whitespace unless values are quoted, so quote
    // every argument rather than hoping paths have no spaces in them.
    let exec_start = definition
        .program_arguments()
        .iter()
        .map(|argument| {
            format!(
                "\"{}\"",
                argument.replace('\\', "\\\\").replace('"', "\\\"")
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[Unit]\n\
         Description=xshell session execution and persistence service\n\
         Documentation=https://github.com/rdevaul/xshell\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec_start}\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition() -> ServiceDefinition {
        ServiceDefinition {
            program: PathBuf::from("/usr/local/bin/xshelld"),
            config: Some(PathBuf::from("/home/ada/.config/xshell/config.toml")),
            log_directory: PathBuf::from("/home/ada/Library/Logs/xshell"),
        }
    }

    #[test]
    fn unit_paths_are_per_user_and_never_system_wide() {
        let home = Path::new("/home/ada");
        let launchd = ServiceManager::Launchd.unit_path(home);
        let systemd = ServiceManager::Systemd.unit_path(home);

        assert_eq!(
            launchd,
            Path::new("/home/ada/Library/LaunchAgents/com.xshell.xshelld.plist")
        );
        assert_eq!(
            systemd,
            Path::new("/home/ada/.config/systemd/user/xshelld.service")
        );
        for path in [&launchd, &systemd] {
            assert!(
                path.starts_with(home),
                "{} escapes the user's home directory",
                path.display()
            );
        }
    }

    /// The daemon does not inherit the shell environment that normally supplies
    /// `XSHELL_CONFIG`, so the unit has to name the config file explicitly or it
    /// will run against different settings than the user's own invocations.
    #[test]
    fn units_pin_the_resolved_configuration_path() {
        let plist = ServiceManager::Launchd.render_unit(&definition());
        let unit = ServiceManager::Systemd.render_unit(&definition());

        for rendered in [&plist, &unit] {
            assert!(rendered.contains("--config"));
            assert!(rendered.contains("/home/ada/.config/xshell/config.toml"));
            assert!(rendered.contains("/usr/local/bin/xshelld"));
        }
    }

    #[test]
    fn units_omit_config_when_no_file_was_resolved() {
        let bare = ServiceDefinition {
            config: None,
            ..definition()
        };
        assert!(
            !ServiceManager::Launchd
                .render_unit(&bare)
                .contains("--config")
        );
        assert!(
            !ServiceManager::Systemd
                .render_unit(&bare)
                .contains("--config")
        );
    }

    /// systemd splits `ExecStart` on whitespace and launchd plists are XML, so
    /// a home directory with a space or an ampersand in it must not corrupt the
    /// unit or inject an extra argument.
    #[test]
    fn hostile_paths_cannot_break_out_of_a_rendered_unit() {
        let awkward = ServiceDefinition {
            program: PathBuf::from("/opt/my tools/xshelld"),
            config: Some(PathBuf::from("/home/a&b/\"quoted\"/config.toml")),
            log_directory: PathBuf::from("/home/a&b/logs"),
        };

        let unit = ServiceManager::Systemd.render_unit(&awkward);
        assert!(
            unit.contains(r#"ExecStart="/opt/my tools/xshelld" "--config""#),
            "each argument must stay one quoted token: {unit}"
        );

        let plist = ServiceManager::Launchd.render_unit(&awkward);
        assert!(plist.contains("/home/a&amp;b/&quot;quoted&quot;/config.toml"));
        assert!(
            !plist.contains("a&b"),
            "raw ampersand would make the plist invalid XML: {plist}"
        );
    }

    #[test]
    fn launchd_install_replaces_a_previous_generation_before_bootstrapping() {
        let plan = ServiceManager::Launchd.install_plan(Path::new("/home/ada"), 501, &definition());
        let rendered: Vec<String> = plan.activate.iter().map(CommandSpec::display).collect();

        assert_eq!(
            rendered,
            [
                "launchctl bootout gui/501/com.xshell.xshelld",
                "launchctl bootstrap gui/501 /home/ada/Library/LaunchAgents/com.xshell.xshelld.plist",
                "launchctl kickstart gui/501/com.xshell.xshelld",
            ]
        );
        // Booting out a service that was never loaded is the normal first-run
        // case, not a failure.
        assert!(plan.activate[0].tolerate_failure);
        assert!(!plan.activate[1].tolerate_failure);
    }

    #[test]
    fn systemd_install_reloads_before_enabling() {
        let plan =
            ServiceManager::Systemd.install_plan(Path::new("/home/ada"), 1000, &definition());
        let rendered: Vec<String> = plan.activate.iter().map(CommandSpec::display).collect();

        assert_eq!(
            rendered,
            [
                "systemctl --user daemon-reload",
                "systemctl --user enable xshelld.service",
                "systemctl --user restart xshelld.service",
            ]
        );
    }

    /// Restart must terminate the running daemon rather than start a second
    /// one: the daemon owns durable sessions and drains them on SIGTERM.
    #[test]
    fn restart_signals_the_running_daemon_rather_than_starting_a_second() {
        let launchd = ServiceManager::Launchd.verb_commands(
            ServiceVerb::Restart,
            501,
            Path::new("/home/ada/Library/LaunchAgents/com.xshell.xshelld.plist"),
        );
        assert_eq!(
            launchd.iter().map(CommandSpec::display).collect::<Vec<_>>(),
            ["launchctl kickstart -k gui/501/com.xshell.xshelld"]
        );

        let systemd =
            ServiceManager::Systemd.verb_commands(ServiceVerb::Restart, 1000, Path::new("/unused"));
        assert_eq!(
            systemd.iter().map(CommandSpec::display).collect::<Vec<_>>(),
            ["systemctl --user restart xshelld.service"]
        );
    }

    #[test]
    fn start_tolerates_an_already_loaded_launchd_agent() {
        let commands = ServiceManager::Launchd.verb_commands(
            ServiceVerb::Start,
            501,
            Path::new("/home/ada/Library/LaunchAgents/com.xshell.xshelld.plist"),
        );
        assert!(
            commands[0].tolerate_failure,
            "bootstrapping a loaded agent fails and must not abort the start"
        );
        assert!(commands[1].display().contains("kickstart"));
        assert!(!commands[1].tolerate_failure);
    }

    #[test]
    fn a_missing_unit_file_is_reported_as_not_installed() {
        for manager in [ServiceManager::Launchd, ServiceManager::Systemd] {
            assert_eq!(
                manager.parse_state(false, true, "anything"),
                ServiceState::NotInstalled
            );
        }
    }

    #[test]
    fn launchd_status_distinguishes_loaded_running_and_unloaded() {
        let manager = ServiceManager::Launchd;
        let running = "\tstate = running\n\tpid = 4242\n\tprogram = /usr/local/bin/xshelld\n";
        assert_eq!(
            manager.parse_state(true, true, running),
            ServiceState::Running { pid: Some(4242) }
        );
        assert_eq!(
            manager.parse_state(true, true, "\tstate = not running\n"),
            ServiceState::Stopped
        );
        // `launchctl print` exits non-zero when the label is not bootstrapped.
        assert_eq!(
            manager.parse_state(true, false, "Could not find service"),
            ServiceState::Installed
        );
    }

    /// Observed on macOS 15: for a moment after `kickstart`, launchd reports
    /// its spawn helper as the state. The service is healthy, so reporting a
    /// failure here would be wrong.
    #[test]
    fn launchd_spawn_transition_is_not_a_failure() {
        let manager = ServiceManager::Launchd;
        let transient = manager.parse_state(true, true, "\tstate = xpcproxy\n");
        assert_eq!(
            transient,
            ServiceState::Starting {
                detail: "xpcproxy".to_owned()
            }
        );
        assert!(transient.is_settling());
        assert!(!matches!(transient, ServiceState::Failed { .. }));

        // Once a pid exists the service is running, whatever launchd calls the
        // internal phase.
        assert_eq!(
            manager.parse_state(true, true, "\tstate = xpcproxy\n\tpid = 99\n"),
            ServiceState::Running { pid: Some(99) }
        );
    }

    /// `launchctl print` nests deeper `state = ...` lines for subsystems. The
    /// service's own state is the first one, and a nested value must not be
    /// mistaken for it.
    #[test]
    fn launchd_status_reads_the_services_own_state_not_a_nested_one() {
        let nested = "\tstate = running\n\tpid = 7\n\tendpoints = {\n\t\tstate = active\n\t}\n";
        assert_eq!(
            ServiceManager::Launchd.parse_state(true, true, nested),
            ServiceState::Running { pid: Some(7) }
        );
    }

    #[test]
    fn systemd_status_maps_active_states_and_ignores_a_zero_pid() {
        let manager = ServiceManager::Systemd;
        assert_eq!(
            manager.parse_state(true, true, "ActiveState=active\nMainPID=1234\n"),
            ServiceState::Running { pid: Some(1234) }
        );
        // systemd reports MainPID=0 for a unit that is not running; that is not
        // a process id.
        assert_eq!(
            manager.parse_state(true, true, "ActiveState=inactive\nMainPID=0\n"),
            ServiceState::Stopped
        );
        assert_eq!(
            manager.parse_state(true, true, "ActiveState=failed\nMainPID=0\n"),
            ServiceState::Failed {
                detail: "unit reported ActiveState=failed".to_owned()
            }
        );
        // Mid-transition is not a settled answer either way.
        assert_eq!(
            manager.parse_state(true, true, "ActiveState=activating\nMainPID=0\n"),
            ServiceState::Starting {
                detail: "activating".to_owned()
            }
        );
        // A unit on disk that `show` does not know about means the manager has
        // not reloaded yet.
        assert_eq!(manager.parse_state(true, true, ""), ServiceState::Installed);
    }

    #[test]
    fn linger_state_is_parsed_from_loginctl_output() {
        assert_eq!(ServiceManager::parse_linger("Linger=yes\n"), Some(true));
        assert_eq!(ServiceManager::parse_linger("Linger=no\n"), Some(false));
        assert_eq!(ServiceManager::parse_linger(""), None);
        assert!(
            ServiceManager::linger_command("ada")
                .display()
                .contains("show-user ada")
        );
    }

    #[test]
    fn uninstall_tears_down_before_the_caller_removes_the_unit() {
        let (teardown, reload) = ServiceManager::Systemd.uninstall_commands(1000);
        assert_eq!(
            teardown[0].display(),
            "systemctl --user disable --now xshelld.service"
        );
        assert!(
            teardown[0].tolerate_failure,
            "disabling a unit that is not loaded must not abort an uninstall"
        );
        assert_eq!(reload[0].display(), "systemctl --user daemon-reload");

        let (teardown, reload) = ServiceManager::Launchd.uninstall_commands(501);
        assert_eq!(
            teardown[0].display(),
            "launchctl bootout gui/501/com.xshell.xshelld"
        );
        assert!(reload.is_empty());
    }

    #[test]
    fn detect_selects_the_manager_for_this_platform() {
        let manager = ServiceManager::detect();
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            let manager = manager.unwrap();
            assert_eq!(
                manager,
                if cfg!(target_os = "macos") {
                    ServiceManager::Launchd
                } else {
                    ServiceManager::Systemd
                }
            );
            assert!(!manager.name().is_empty());
        } else {
            assert!(manager.is_err());
        }
    }
}

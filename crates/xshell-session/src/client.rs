use crate::{
    ApprovalReply, AttachmentRole, ClientRequest, DAEMON_PROBE_SCHEMA_VERSION, DaemonProbeReport,
    DaemonProbeStatus, EventBatch, ModelBinding, PtySize, PtyTicket, SESSION_PROTOCOL_VERSION,
    ServerResponse, SessionCreation, SessionDescriptor, SessionSnapshot, ShellCompletionResult,
    TurnInput, ViewResource,
};
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use xshell_core::ChatMessage;
use xshell_execution::{ApprovalDecision, ApprovalPolicy};

const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const REMOTE_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REMOTE_PROBE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionHandshake {
    Opened {
        protocol_version: u32,
        host_id: String,
        host_alias: String,
        user: String,
    },
    Rejected {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteDaemonProbe {
    MissingBinary,
    Report(DaemonProbeReport),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteBootstrapAction {
    Connect,
    Install,
    Upgrade {
        binary_version: String,
        supported_protocol_version: u32,
    },
    Start,
    Restart {
        reason: String,
    },
    Rejected {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteTarget {
    Aarch64AppleDarwin,
    X86_64AppleDarwin,
    Aarch64UnknownLinuxMusl,
    X86_64UnknownLinuxMusl,
}

impl RemoteTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aarch64AppleDarwin => "aarch64-apple-darwin",
            Self::X86_64AppleDarwin => "x86_64-apple-darwin",
            Self::Aarch64UnknownLinuxMusl => "aarch64-unknown-linux-musl",
            Self::X86_64UnknownLinuxMusl => "x86_64-unknown-linux-musl",
        }
    }
}

impl RemoteDaemonProbe {
    pub fn required_action(&self) -> RemoteBootstrapAction {
        let Self::Report(report) = self else {
            return RemoteBootstrapAction::Install;
        };
        if report.supported_protocol_version != SESSION_PROTOCOL_VERSION {
            return RemoteBootstrapAction::Upgrade {
                binary_version: report.binary_version.clone(),
                supported_protocol_version: report.supported_protocol_version,
            };
        }
        match &report.daemon {
            DaemonProbeStatus::Ready {
                protocol_version, ..
            } if *protocol_version == SESSION_PROTOCOL_VERSION => RemoteBootstrapAction::Connect,
            DaemonProbeStatus::Ready {
                protocol_version, ..
            } => RemoteBootstrapAction::Restart {
                reason: format!(
                    "running daemon speaks protocol {protocol_version}, but the installed binary supports {SESSION_PROTOCOL_VERSION}"
                ),
            },
            DaemonProbeStatus::Incompatible { message, .. } => RemoteBootstrapAction::Restart {
                reason: message.clone(),
            },
            DaemonProbeStatus::Unavailable { .. } => RemoteBootstrapAction::Start,
            DaemonProbeStatus::Rejected { code, message } => RemoteBootstrapAction::Rejected {
                code: code.clone(),
                message: message.clone(),
            },
        }
    }
}

pub struct SessionClient {
    client_id: String,
    host_id: String,
    host_alias: String,
    user: String,
    reader: BufReader<Box<dyn Read + Send>>,
    writer: Box<dyn Write + Send>,
    _transport: TransportGuard,
}

enum TransportGuard {
    Local,
    Ssh(Child),
}

impl Drop for TransportGuard {
    fn drop(&mut self) {
        if let Self::Ssh(child) = self {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl SessionClient {
    /// Probe a local daemon without attaching to or mutating any session.
    /// Unlike `connect`, this preserves a protocol rejection as structured
    /// data so a bootstrap client can distinguish incompatibility from an
    /// unavailable service.
    pub fn probe(socket: &Path, client_version: &str) -> Result<SessionHandshake> {
        let stream = UnixStream::connect(socket).with_context(|| {
            format!(
                "cannot connect to xshell session service at {}",
                socket.display()
            )
        })?;
        set_close_on_exec(&stream)?;
        stream
            .set_read_timeout(Some(PROBE_TIMEOUT))
            .context("cannot set session probe read timeout")?;
        stream
            .set_write_timeout(Some(PROBE_TIMEOUT))
            .context("cannot set session probe write timeout")?;
        let mut writer = stream.try_clone().context("cannot clone session socket")?;
        set_close_on_exec(&writer)?;
        serde_json::to_writer(&mut writer, &ClientRequest::open(client_version))?;
        writer.write_all(b"\n")?;
        writer.flush().context("cannot flush session probe")?;
        let mut reader = BufReader::new(stream);
        match receive_response(&mut reader)? {
            ServerResponse::Opened {
                protocol_version,
                host_id,
                host_alias,
                user,
                ..
            } => Ok(SessionHandshake::Opened {
                protocol_version,
                host_id,
                host_alias,
                user,
            }),
            ServerResponse::Error { code, message } => {
                Ok(SessionHandshake::Rejected { code, message })
            }
            response => bail!("unexpected session probe response: {response:?}"),
        }
    }

    pub fn connect(socket: &Path, client_version: &str) -> Result<Self> {
        let stream = UnixStream::connect(socket).with_context(|| {
            format!(
                "cannot connect to xshell session service at {}",
                socket.display()
            )
        })?;
        set_close_on_exec(&stream)?;
        let writer = stream.try_clone().context("cannot clone session socket")?;
        set_close_on_exec(&writer)?;
        Self::open(
            Box::new(stream),
            Box::new(writer),
            TransportGuard::Local,
            client_version,
        )
    }

    /// Discover an installed remote binary and its running daemon without
    /// attaching to or mutating a session. A missing `xshelld` is represented
    /// as data so the caller can offer an explicit bootstrap operation.
    pub fn probe_ssh(destination: &str) -> Result<RemoteDaemonProbe> {
        validate_ssh_destination(destination)?;
        let (status, bytes) = run_bounded_ssh(
            destination,
            &["xshelld", "probe"],
            MAX_REMOTE_PROBE_BYTES,
            REMOTE_PROBE_TIMEOUT,
            "xshelld probe",
        )?;
        parse_remote_probe(status, &bytes)
            .with_context(|| format!("invalid xshelld probe response from {destination:?}"))
    }

    /// Detect the release target for a remote macOS or Linux host without
    /// changing remote state or relying on shell startup files.
    pub fn detect_ssh_target(destination: &str) -> Result<RemoteTarget> {
        validate_ssh_destination(destination)?;
        let (status, bytes) = run_bounded_ssh(
            destination,
            &["uname", "-sm"],
            1024,
            REMOTE_PROBE_TIMEOUT,
            "platform probe",
        )?;
        if !status.success() {
            bail!("remote platform probe exited with {status}");
        }
        parse_remote_target(&bytes)
            .with_context(|| format!("unsupported platform reported by {destination:?}"))
    }

    pub fn connect_ssh(destination: &str, client_version: &str) -> Result<Self> {
        validate_ssh_destination(destination)?;
        let mut child = Command::new("ssh")
            .args(["-T", "--", destination, "xshelld", "serve-stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("cannot start ssh connection to {destination:?}"))?;
        let reader = child.stdout.take().context("cannot capture ssh stdout")?;
        let writer = child.stdin.take().context("cannot open ssh stdin")?;
        Self::open(
            Box::new(reader),
            Box::new(writer),
            TransportGuard::Ssh(child),
            client_version,
        )
        .with_context(|| format!("cannot open xshell session service on {destination:?}"))
    }

    fn open(
        reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
        transport: TransportGuard,
        client_version: &str,
    ) -> Result<Self> {
        let mut client = Self {
            client_id: String::new(),
            host_id: String::new(),
            host_alias: String::new(),
            user: String::new(),
            reader: BufReader::new(reader),
            writer,
            _transport: transport,
        };
        client.send(&ClientRequest::open(client_version))?;
        match client.receive()? {
            ServerResponse::Opened {
                protocol_version,
                client_id,
                host_id,
                host_alias,
                user,
            } => {
                if protocol_version != SESSION_PROTOCOL_VERSION {
                    bail!(
                        "session service speaks protocol {protocol_version} but this client requires {SESSION_PROTOCOL_VERSION}; \
                         the running xshelld is a different build than this xshell — restart xshelld from the same build"
                    );
                }
                client.client_id = client_id;
                client.host_id = host_id;
                client.host_alias = host_alias;
                client.user = user;
                Ok(client)
            }
            response => response_error("open", response),
        }
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    pub fn host_alias(&self) -> &str {
        &self.host_alias
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn list(&mut self) -> Result<Vec<SessionDescriptor>> {
        self.send(&ClientRequest::List)?;
        match self.receive()? {
            ServerResponse::Catalog { sessions } => Ok(sessions),
            response => response_error("list", response),
        }
    }

    pub fn create(&mut self, session: SessionCreation) -> Result<SessionSnapshot> {
        self.send(&ClientRequest::Create { session })?;
        match self.receive()? {
            ServerResponse::Created { session } => Ok(session),
            response => response_error("create", response),
        }
    }

    pub fn attach(&mut self, selector: String) -> Result<SessionSnapshot> {
        self.send(&ClientRequest::Attach {
            selector,
            role: AttachmentRole::Owner,
        })?;
        match self.receive()? {
            ServerResponse::Attached { session, .. } => Ok(session),
            response => response_error("attach", response),
        }
    }

    pub fn switch(&mut self, selector: String) -> Result<SessionSnapshot> {
        self.send(&ClientRequest::Switch {
            selector,
            role: AttachmentRole::Owner,
        })?;
        match self.receive()? {
            ServerResponse::Attached { session, .. } => Ok(session),
            response => response_error("switch", response),
        }
    }

    pub fn update(
        &mut self,
        session_id: String,
        model: ModelBinding,
        cwd: PathBuf,
        history: Vec<ChatMessage>,
    ) -> Result<SessionDescriptor> {
        self.send(&ClientRequest::Update {
            session_id,
            model,
            cwd,
            history,
        })?;
        match self.receive()? {
            ServerResponse::Updated { session } => Ok(session),
            response => response_error("update", response),
        }
    }

    pub fn snapshot(&mut self, session_id: String) -> Result<SessionSnapshot> {
        self.send(&ClientRequest::Snapshot { session_id })?;
        match self.receive()? {
            ServerResponse::Snapshot { session } => Ok(session),
            response => response_error("snapshot", response),
        }
    }

    pub fn submit(
        &mut self,
        session_id: String,
        input: TurnInput,
        approval: ApprovalPolicy,
    ) -> Result<String> {
        self.send(&ClientRequest::Submit {
            session_id,
            input,
            approval,
        })?;
        match self.receive()? {
            ServerResponse::Accepted { turn_id } => Ok(turn_id),
            response => response_error("submit", response),
        }
    }

    pub fn events(
        &mut self,
        session_id: String,
        after_sequence: u64,
        wait_ms: u64,
    ) -> Result<EventBatch> {
        self.send(&ClientRequest::Events {
            session_id,
            after_sequence,
            wait_ms,
        })?;
        match self.receive()? {
            ServerResponse::Events { batch } => Ok(batch),
            response => response_error("events", response),
        }
    }

    pub fn approve(
        &mut self,
        session_id: String,
        turn_id: String,
        call_id: String,
        decision: ApprovalDecision,
    ) -> Result<()> {
        self.send(&ClientRequest::Approve {
            session_id,
            reply: ApprovalReply {
                turn_id,
                call_id,
                decision,
            },
        })?;
        match self.receive()? {
            ServerResponse::ApprovalAccepted => Ok(()),
            response => response_error("approval", response),
        }
    }

    pub fn cancel(&mut self, session_id: String, turn_id: String) -> Result<()> {
        self.send(&ClientRequest::Cancel {
            session_id,
            turn_id,
        })?;
        match self.receive()? {
            ServerResponse::CancellationAccepted => Ok(()),
            response => response_error("cancel", response),
        }
    }

    pub fn complete_shell(
        &mut self,
        session_id: String,
        line: String,
        cursor: usize,
    ) -> Result<ShellCompletionResult> {
        self.send(&ClientRequest::CompleteShell {
            session_id,
            line,
            cursor,
        })?;
        match self.receive()? {
            ServerResponse::ShellCompletions { result } => Ok(result),
            response => response_error("shell completion", response),
        }
    }

    pub fn view_source(&mut self, session_id: String, path: PathBuf) -> Result<ViewResource> {
        self.send(&ClientRequest::ViewSource { session_id, path })?;
        match self.receive()? {
            ServerResponse::ViewSource { resource } => Ok(resource),
            response => response_error("view source", response),
        }
    }

    pub fn pty_start(
        &mut self,
        session_id: String,
        command: String,
        size: PtySize,
        terminal_type: Option<String>,
    ) -> Result<PtyTicket> {
        self.send(&ClientRequest::PtyStart {
            session_id,
            command,
            size,
            terminal_type,
        })?;
        match self.receive()? {
            ServerResponse::PtyStarted { ticket } => Ok(ticket),
            response => response_error("PTY start", response),
        }
    }

    pub fn pty_attach(
        &mut self,
        session_id: String,
        after_offset: Option<u64>,
    ) -> Result<PtyTicket> {
        self.send(&ClientRequest::PtyAttach {
            session_id,
            after_offset,
        })?;
        match self.receive()? {
            ServerResponse::PtyAttached { ticket } => Ok(ticket),
            response => response_error("PTY attach", response),
        }
    }

    pub fn pty_close(&mut self, session_id: String) -> Result<()> {
        self.send(&ClientRequest::PtyClose { session_id })?;
        match self.receive()? {
            ServerResponse::PtyClosed => Ok(()),
            response => response_error("PTY close", response),
        }
    }

    pub fn detach(&mut self) -> Result<Option<String>> {
        self.send(&ClientRequest::Detach)?;
        match self.receive()? {
            ServerResponse::Detached { session_id } => Ok(session_id),
            response => response_error("detach", response),
        }
    }

    pub fn close(&mut self, selector: Option<String>) -> Result<String> {
        self.send(&ClientRequest::Close { selector })?;
        match self.receive()? {
            ServerResponse::Closed { session_id } => Ok(session_id),
            response => response_error("close", response),
        }
    }

    fn send(&mut self, request: &ClientRequest) -> Result<()> {
        serde_json::to_writer(&mut self.writer, request)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush().context("cannot flush session request")
    }

    fn receive(&mut self) -> Result<ServerResponse> {
        receive_response(&mut self.reader)
    }
}

fn validate_ssh_destination(destination: &str) -> Result<()> {
    if destination.trim().is_empty() || destination.starts_with('-') {
        bail!("SSH destination must be non-empty and may not begin with '-'");
    }
    Ok(())
}

fn run_bounded_ssh(
    destination: &str,
    remote_command: &[&str],
    maximum_bytes: usize,
    timeout: Duration,
    operation: &str,
) -> Result<(ExitStatus, Vec<u8>)> {
    let mut child = Command::new("ssh")
        .args(["-T", "--", destination])
        .args(remote_command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("cannot start SSH {operation} for {destination:?}"))?;
    let stdout = child
        .stdout
        .take()
        .with_context(|| format!("cannot capture SSH {operation} stdout"))?;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take((maximum_bytes + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("cannot poll SSH {operation}"))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            bail!("SSH {operation} for {destination:?} timed out");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let bytes = reader
        .join()
        .map_err(|_| anyhow::anyhow!("SSH {operation} output reader panicked"))??;
    if bytes.len() > maximum_bytes {
        bail!("SSH {operation} response exceeds {maximum_bytes} bytes");
    }
    Ok((status, bytes))
}

fn parse_remote_target(bytes: &[u8]) -> Result<RemoteTarget> {
    let output = std::str::from_utf8(bytes).context("platform probe returned non-UTF-8 output")?;
    let fields = output.split_whitespace().collect::<Vec<_>>();
    let [system, architecture] = fields.as_slice() else {
        bail!("expected `uname -sm` to return an operating system and architecture");
    };
    match (*system, *architecture) {
        ("Darwin", "arm64" | "aarch64") => Ok(RemoteTarget::Aarch64AppleDarwin),
        ("Darwin", "x86_64" | "amd64") => Ok(RemoteTarget::X86_64AppleDarwin),
        ("Linux", "arm64" | "aarch64") => Ok(RemoteTarget::Aarch64UnknownLinuxMusl),
        ("Linux", "x86_64" | "amd64") => Ok(RemoteTarget::X86_64UnknownLinuxMusl),
        _ => bail!("unsupported remote platform {system} {architecture}"),
    }
}

fn parse_remote_probe(status: ExitStatus, bytes: &[u8]) -> Result<RemoteDaemonProbe> {
    if bytes.len() > MAX_REMOTE_PROBE_BYTES {
        bail!("remote probe response exceeds {MAX_REMOTE_PROBE_BYTES} bytes");
    }
    if !status.success() {
        if status.code() == Some(127) && bytes.is_empty() {
            return Ok(RemoteDaemonProbe::MissingBinary);
        }
        bail!("remote probe exited with {status}");
    }
    let report: DaemonProbeReport = serde_json::from_slice(bytes)
        .context("remote probe did not return its JSON capability report")?;
    if report.schema_version != DAEMON_PROBE_SCHEMA_VERSION {
        bail!(
            "remote probe uses schema {}, but this client requires {}",
            report.schema_version,
            DAEMON_PROBE_SCHEMA_VERSION
        );
    }
    Ok(RemoteDaemonProbe::Report(report))
}

fn receive_response(reader: &mut dyn BufRead) -> Result<ServerResponse> {
    let mut bytes = Vec::new();
    let mut limited = reader.take((MAX_RESPONSE_BYTES + 1) as u64);
    let count = limited.read_until(b'\n', &mut bytes)?;
    if count == 0 {
        bail!("session service closed the connection");
    }
    if bytes.len() > MAX_RESPONSE_BYTES || bytes.last() != Some(&b'\n') {
        bail!("session service response exceeds {MAX_RESPONSE_BYTES} bytes");
    }
    serde_json::from_slice(&bytes).context("session service returned an invalid response")
}

fn response_error<T>(operation: &str, response: ServerResponse) -> Result<T> {
    match response {
        ServerResponse::Error { code, message } => {
            bail!("session {operation} failed ({code}): {message}")
        }
        response => bail!("unexpected session {operation} response: {response:?}"),
    }
}

fn set_close_on_exec(stream: &UnixStream) -> Result<()> {
    let descriptor = stream.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("cannot inspect session socket flags");
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error())
            .context("cannot protect session socket from child processes");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn report(protocol: u32, daemon: DaemonProbeStatus) -> RemoteDaemonProbe {
        RemoteDaemonProbe::Report(DaemonProbeReport {
            schema_version: DAEMON_PROBE_SCHEMA_VERSION,
            binary_version: "0.2.0".into(),
            supported_protocol_version: protocol,
            daemon,
        })
    }

    #[test]
    fn bootstrap_decision_matrix_distinguishes_remote_repairs() {
        assert_eq!(
            RemoteDaemonProbe::MissingBinary.required_action(),
            RemoteBootstrapAction::Install
        );
        assert_eq!(
            report(
                SESSION_PROTOCOL_VERSION - 1,
                DaemonProbeStatus::Unavailable {
                    message: "not running".into(),
                },
            )
            .required_action(),
            RemoteBootstrapAction::Upgrade {
                binary_version: "0.2.0".into(),
                supported_protocol_version: SESSION_PROTOCOL_VERSION - 1,
            }
        );
        assert_eq!(
            report(
                SESSION_PROTOCOL_VERSION,
                DaemonProbeStatus::Unavailable {
                    message: "not running".into(),
                },
            )
            .required_action(),
            RemoteBootstrapAction::Start
        );
        assert!(matches!(
            report(
                SESSION_PROTOCOL_VERSION,
                DaemonProbeStatus::Incompatible {
                    code: "protocol_version".into(),
                    message: "old daemon".into(),
                },
            )
            .required_action(),
            RemoteBootstrapAction::Restart { reason } if reason == "old daemon"
        ));
        assert_eq!(
            report(
                SESSION_PROTOCOL_VERSION,
                DaemonProbeStatus::Ready {
                    protocol_version: SESSION_PROTOCOL_VERSION,
                    host_id: "host-id".into(),
                    host_alias: "host".into(),
                    user: "user".into(),
                },
            )
            .required_action(),
            RemoteBootstrapAction::Connect
        );
        assert_eq!(
            report(
                SESSION_PROTOCOL_VERSION,
                DaemonProbeStatus::Rejected {
                    code: "policy".into(),
                    message: "denied".into(),
                },
            )
            .required_action(),
            RemoteBootstrapAction::Rejected {
                code: "policy".into(),
                message: "denied".into(),
            }
        );
    }

    #[test]
    fn parses_missing_binary_and_versioned_report() {
        let missing = ExitStatus::from_raw(127 << 8);
        assert_eq!(
            parse_remote_probe(missing, b"").unwrap(),
            RemoteDaemonProbe::MissingBinary
        );

        let bytes = serde_json::to_vec(&DaemonProbeReport {
            schema_version: DAEMON_PROBE_SCHEMA_VERSION,
            binary_version: "0.2.0".into(),
            supported_protocol_version: SESSION_PROTOCOL_VERSION,
            daemon: DaemonProbeStatus::Unavailable {
                message: "socket missing".into(),
            },
        })
        .unwrap();
        assert!(matches!(
            parse_remote_probe(ExitStatus::from_raw(0), &bytes).unwrap(),
            RemoteDaemonProbe::Report(DaemonProbeReport {
                daemon: DaemonProbeStatus::Unavailable { .. },
                ..
            })
        ));
    }

    #[test]
    fn rejects_unknown_or_oversized_probe_responses() {
        let unknown_schema = br#"{"schema_version":2,"binary_version":"0.2.0","supported_protocol_version":11,"daemon_status":"unavailable","message":"missing"}"#;
        assert!(
            parse_remote_probe(ExitStatus::from_raw(0), unknown_schema)
                .unwrap_err()
                .to_string()
                .contains("requires 1")
        );
        assert!(
            parse_remote_probe(
                ExitStatus::from_raw(0),
                &vec![b'x'; MAX_REMOTE_PROBE_BYTES + 1]
            )
            .unwrap_err()
            .to_string()
            .contains("exceeds")
        );
    }

    #[test]
    fn maps_supported_uname_outputs_to_release_targets() {
        assert_eq!(
            parse_remote_target(b"Darwin arm64\n").unwrap().as_str(),
            "aarch64-apple-darwin"
        );
        assert_eq!(
            parse_remote_target(b"Linux x86_64\n").unwrap().as_str(),
            "x86_64-unknown-linux-musl"
        );
        assert!(parse_remote_target(b"FreeBSD amd64\n").is_err());
        assert!(parse_remote_target(b"Linux\n").is_err());
    }
}

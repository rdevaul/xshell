use crate::{
    ApprovalReply, AttachmentRole, ClientRequest, EventBatch, ModelBinding, PtyDescriptor, PtySize,
    PtyTicket, SESSION_PROTOCOL_VERSION, ServerResponse, SessionCreation, SessionDescriptor,
    SessionSnapshot, ShellCompletionResult, TurnInput, ViewResource,
};
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use xshell_core::ChatMessage;
use xshell_execution::{ApprovalDecision, ApprovalPolicy};

const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

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

    pub fn connect_ssh(destination: &str, client_version: &str) -> Result<Self> {
        if destination.trim().is_empty() || destination.starts_with('-') {
            bail!("SSH destination must be non-empty and may not begin with '-'");
        }
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

    pub fn pty_list(&mut self) -> Result<Vec<PtyDescriptor>> {
        self.send(&ClientRequest::PtyList)?;
        match self.receive()? {
            ServerResponse::PtyCatalog { ptys } => Ok(ptys),
            response => response_error("PTY list", response),
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

    pub fn pty_close(&mut self, pty_id: String) -> Result<()> {
        self.send(&ClientRequest::PtyClose { pty_id })?;
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
        let mut bytes = Vec::new();
        let count = self
            .reader
            .by_ref()
            .take((MAX_RESPONSE_BYTES + 1) as u64)
            .read_until(b'\n', &mut bytes)?;
        if count == 0 {
            bail!("session service closed the connection");
        }
        if bytes.len() > MAX_RESPONSE_BYTES || bytes.last() != Some(&b'\n') {
            bail!("session service response exceeds {MAX_RESPONSE_BYTES} bytes");
        }
        serde_json::from_slice(&bytes).context("session service returned an invalid response")
    }
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

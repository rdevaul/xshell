use crate::{
    ApprovalReply, AttachmentRole, ClientRequest, EventBatch, ModelBinding,
    SESSION_PROTOCOL_VERSION, ServerResponse, SessionCreation, SessionDescriptor, SessionSnapshot,
    TurnInput,
};
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use xshell_core::ChatMessage;
use xshell_execution::{ApprovalDecision, ApprovalPolicy};

const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

pub struct SessionClient {
    client_id: String,
    host_id: String,
    host_alias: String,
    user: String,
    reader: BufReader<UnixStream>,
    writer: UnixStream,
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
        let mut client = Self {
            client_id: String::new(),
            host_id: String::new(),
            host_alias: String::new(),
            user: String::new(),
            reader: BufReader::new(stream),
            writer,
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
                    bail!("session service selected unsupported protocol {protocol_version}");
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

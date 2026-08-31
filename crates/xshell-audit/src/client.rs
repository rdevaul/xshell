use crate::{AUDIT_PROTOCOL_VERSION, AuditCheckpoint, AuditEvent, ClientRequest, ServerResponse};
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

pub struct AuditClient {
    session_id: String,
    signing_key_id: String,
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl AuditClient {
    pub fn connect(socket: &Path, client_version: &str) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .with_context(|| format!("cannot connect to audit service at {}", socket.display()))?;
        set_close_on_exec(&stream)?;
        let writer = stream.try_clone().context("cannot clone audit socket")?;
        set_close_on_exec(&writer)?;
        let mut client = Self {
            session_id: String::new(),
            signing_key_id: String::new(),
            reader: BufReader::new(stream),
            writer,
        };
        client.send(&ClientRequest::open(client_version))?;
        match client.receive()? {
            ServerResponse::Opened {
                protocol_version,
                session_id,
                signing_key_id,
            } => {
                if protocol_version != AUDIT_PROTOCOL_VERSION {
                    bail!("audit service selected unsupported protocol {protocol_version}");
                }
                client.session_id = session_id;
                client.signing_key_id = signing_key_id;
                Ok(client)
            }
            ServerResponse::Error { message } => bail!("audit service rejected session: {message}"),
            response => bail!("unexpected audit handshake response: {response:?}"),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn signing_key_id(&self) -> &str {
        &self.signing_key_id
    }

    pub fn append(&mut self, event: AuditEvent) -> Result<(u64, String)> {
        self.send(&ClientRequest::Append { event })?;
        match self.receive()? {
            ServerResponse::Ack {
                sequence,
                record_hash,
            } => Ok((sequence, record_hash)),
            ServerResponse::Error { message } => bail!("audit append failed: {message}"),
            response => bail!("unexpected audit append response: {response:?}"),
        }
    }

    pub fn close(mut self) -> Result<AuditCheckpoint> {
        self.send(&ClientRequest::Close)?;
        match self.receive()? {
            ServerResponse::Closed { checkpoint } => Ok(checkpoint),
            ServerResponse::Error { message } => bail!("audit close failed: {message}"),
            response => bail!("unexpected audit close response: {response:?}"),
        }
    }

    fn send(&mut self, request: &ClientRequest) -> Result<()> {
        serde_json::to_writer(&mut self.writer, request)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush().context("cannot flush audit request")
    }

    fn receive(&mut self) -> Result<ServerResponse> {
        let mut bytes = Vec::new();
        let count = self
            .reader
            .by_ref()
            .take((MAX_RESPONSE_BYTES + 1) as u64)
            .read_until(b'\n', &mut bytes)?;
        if count == 0 {
            bail!("audit service closed the connection");
        }
        if bytes.len() > MAX_RESPONSE_BYTES || bytes.last() != Some(&b'\n') {
            bail!("audit service response exceeds {MAX_RESPONSE_BYTES} bytes");
        }
        serde_json::from_slice(&bytes).context("audit service returned an invalid response")
    }
}

fn set_close_on_exec(stream: &UnixStream) -> Result<()> {
    let descriptor = stream.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("cannot inspect audit socket flags");
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error())
            .context("cannot protect audit socket from child processes");
    }
    Ok(())
}

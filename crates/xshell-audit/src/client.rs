use crate::{AUDIT_PROTOCOL_VERSION, AuditCheckpoint, AuditEvent, ClientRequest, ServerResponse};
use anyhow::{Context, Result, bail};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// The deadline [`AuditClient::connect`] applies to each exchange.
pub const DEFAULT_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

/// A connection to `xshell-auditd`.
///
/// Every exchange (open, append with its acknowledgement, close) must finish
/// within the client's deadline. The deadline covers the whole exchange, so a
/// peer that trickles bytes cannot stretch it. After any failed exchange,
/// including a missed deadline, the connection's state is unknown: the
/// record may or may not have been written. The client then refuses further
/// use, and the caller must treat the record as not acknowledged.
pub struct AuditClient {
    session_id: String,
    signing_key_id: String,
    reader: BufReader<DeadlineStream>,
    writer: DeadlineStream,
    timeout: Duration,
    failed: bool,
}

impl AuditClient {
    pub fn connect(socket: &Path, client_version: &str) -> Result<Self> {
        Self::connect_with_timeout(socket, client_version, DEFAULT_EXCHANGE_TIMEOUT)
    }

    pub fn probe(socket: &Path, client_version: &str) -> Result<()> {
        Self::connect_with_timeout(socket, client_version, Duration::from_secs(2))?.close()?;
        Ok(())
    }

    /// Connect with `timeout` as the deadline for each exchange, the opening
    /// handshake included.
    pub fn connect_with_timeout(
        socket: &Path,
        client_version: &str,
        timeout: Duration,
    ) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .with_context(|| format!("cannot connect to audit service at {}", socket.display()))?;
        set_close_on_exec(&stream)?;
        let writer = stream.try_clone().context("cannot clone audit socket")?;
        set_close_on_exec(&writer)?;
        let mut client = Self {
            session_id: String::new(),
            signing_key_id: String::new(),
            reader: BufReader::new(DeadlineStream::new(stream)),
            writer: DeadlineStream::new(writer),
            timeout,
            failed: false,
        };
        match client.exchange(&ClientRequest::open(client_version))? {
            ServerResponse::Opened {
                protocol_version,
                session_id,
                signing_key_id,
            } => {
                if protocol_version != AUDIT_PROTOCOL_VERSION {
                    bail!(
                        "audit service speaks protocol {protocol_version} but this client requires {AUDIT_PROTOCOL_VERSION}; \
                         the running xshell-auditd is a different build — restart xshell-auditd from the same build"
                    );
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
        match self.exchange(&ClientRequest::Append { event })? {
            ServerResponse::Ack {
                sequence,
                record_hash,
            } => Ok((sequence, record_hash)),
            ServerResponse::Error { message } => bail!("audit append failed: {message}"),
            response => bail!("unexpected audit append response: {response:?}"),
        }
    }

    pub fn close(mut self) -> Result<AuditCheckpoint> {
        match self.exchange(&ClientRequest::Close)? {
            ServerResponse::Closed { checkpoint } => Ok(checkpoint),
            ServerResponse::Error { message } => bail!("audit close failed: {message}"),
            response => bail!("unexpected audit close response: {response:?}"),
        }
    }

    /// Send `request` and receive its response within the deadline. Any
    /// failure leaves the connection unusable.
    fn exchange(&mut self, request: &ClientRequest) -> Result<ServerResponse> {
        if self.failed {
            bail!("the audit connection failed earlier and cannot be reused");
        }
        let deadline = Instant::now() + self.timeout;
        self.writer.deadline = Some(deadline);
        self.reader.get_mut().deadline = Some(deadline);
        let result = self.send(request).and_then(|()| self.receive());
        self.writer.deadline = None;
        self.reader.get_mut().deadline = None;
        if result.is_err() {
            self.failed = true;
        }
        result.map_err(|error| {
            if timed_out(&error) {
                error.context(format!(
                    "the audit service did not respond within {} ms",
                    self.timeout.as_millis()
                ))
            } else {
                error
            }
        })
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

fn timed_out(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            )
        }) || cause
            .downcast_ref::<serde_json::Error>()
            .and_then(|error| error.io_error_kind())
            .is_some_and(|kind| matches!(kind, io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock))
    })
}

/// A socket whose reads and writes fail once `deadline` has passed. Each call
/// gets only the time remaining, so the deadline bounds a whole exchange
/// however the peer splits its bytes.
struct DeadlineStream {
    stream: UnixStream,
    deadline: Option<Instant>,
}

impl DeadlineStream {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            deadline: None,
        }
    }

    fn remaining(&self) -> io::Result<Option<Duration>> {
        let Some(deadline) = self.deadline else {
            return Ok(None);
        };
        match deadline.checked_duration_since(Instant::now()) {
            Some(remaining) if !remaining.is_zero() => Ok(Some(remaining)),
            _ => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "audit exchange deadline elapsed",
            )),
        }
    }
}

impl Read for DeadlineStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let timeout = self.remaining()?;
        tolerate_closed_peer(self.stream.set_read_timeout(timeout))?;
        self.stream.read(buffer)
    }
}

impl Write for DeadlineStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let timeout = self.remaining()?;
        tolerate_closed_peer(self.stream.set_write_timeout(timeout))?;
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// macOS refuses to set a socket timeout with `EINVAL` once the peer has
/// closed the connection, which the audit service does right after its final
/// reply. A socket in that state cannot block: the read returns the buffered
/// reply or end of file, and a write fails. So the timeout is not needed, and
/// the I/O itself reports the socket's real state.
fn tolerate_closed_peer(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.raw_os_error() == Some(libc::EINVAL) => Ok(()),
        result => result,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Shutdown;

    /// The audit service closes its end right after the final reply. Reading
    /// that reply must not fail, though macOS will not set a timeout on a
    /// socket whose peer has closed.
    #[test]
    fn a_reply_followed_by_close_is_still_read_within_the_deadline() {
        let (client, mut service) = UnixStream::pair().unwrap();
        service.write_all(b"{\"response\":\"ack\"}\n").unwrap();
        service.shutdown(Shutdown::Both).unwrap();
        drop(service);

        let mut reader = BufReader::new(DeadlineStream::new(client));
        reader.get_mut().deadline = Some(Instant::now() + Duration::from_secs(5));
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, "{\"response\":\"ack\"}\n");
        line.clear();
        assert_eq!(
            reader.read_line(&mut line).unwrap(),
            0,
            "end of file after the reply"
        );
    }

    #[test]
    fn a_missed_deadline_is_a_timeout() {
        let (client, _service) = UnixStream::pair().unwrap();
        let mut stream = DeadlineStream::new(client);
        stream.deadline = Some(Instant::now() + Duration::from_millis(50));
        let started = Instant::now();
        let error = stream.read(&mut [0; 8]).unwrap_err();
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ),
            "{error:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}

use crate::{ClientRequest, PtySize, SESSION_PROTOCOL_VERSION, ServerResponse};
use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use xshell_pty::{DuplexPtyCommand, DuplexPtyEvent, DuplexPtyOutcome};

const MAX_FRAME_BYTES: usize = 256 * 1024;
const MAX_TEXT_BYTES: usize = 4 * 1024;
const MAX_JSON_BYTES: usize = 64 * 1024;
const TAG_INPUT: u8 = 1;
const TAG_RESIZE: u8 = 2;
const TAG_CLOSE: u8 = 3;
const TAG_READY: u8 = 0x80;
const TAG_OUTPUT: u8 = 0x81;
const TAG_EXIT: u8 = 0x82;
const TAG_ERROR: u8 = 0x83;
const TAG_DETACHED: u8 = 0x84;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientPtyFrame {
    Input(Vec<u8>),
    Resize(PtySize),
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerPtyFrame {
    Ready,
    Output { offset: u64, bytes: Vec<u8> },
    Exit(String),
    Error(String),
    Detached,
}

trait ReadFd: Read + AsRawFd {}
impl<T: Read + AsRawFd> ReadFd for T {}

enum TransportGuard {
    Local,
    Ssh(Child),
}

pub struct PtyStreamClient {
    reader: Box<dyn ReadFd + Send>,
    writer: Box<dyn Write + Send>,
    transport: TransportGuard,
    cursor: u64,
}

impl PtyStreamClient {
    pub fn connect_local(socket: &Path, ticket: &str, cursor: u64) -> Result<Self> {
        let reader = UnixStream::connect(socket).with_context(|| {
            format!(
                "cannot connect PTY stream to daemon at {}",
                socket.display()
            )
        })?;
        let mut writer = reader.try_clone().context("cannot clone PTY socket")?;
        send_json(&mut writer, &ClientRequest::open(env!("CARGO_PKG_VERSION")))?;
        match read_json_response(&mut &reader)? {
            ServerResponse::Opened {
                protocol_version, ..
            } if protocol_version == SESSION_PROTOCOL_VERSION => {}
            response => bail!("PTY stream could not open daemon connection: {response:?}"),
        }
        send_json(
            &mut writer,
            &ClientRequest::PtyClaim {
                ticket: ticket.to_owned(),
            },
        )?;
        match read_json_response(&mut &reader)? {
            ServerResponse::PtyClaimed => Ok(Self {
                reader: Box::new(reader),
                writer: Box::new(writer),
                transport: TransportGuard::Local,
                cursor,
            }),
            ServerResponse::Error { message, .. } => bail!("PTY claim failed: {message}"),
            response => bail!("unexpected PTY claim response: {response:?}"),
        }
    }

    pub fn connect_ssh(destination: &str, ticket: &str, cursor: u64) -> Result<Self> {
        if destination.trim().is_empty() || destination.starts_with('-') {
            bail!("SSH destination must be non-empty and may not begin with '-'");
        }
        let mut child = Command::new("ssh")
            .args(["-T", "--", destination, "xshelld", "serve-pty-stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("cannot start PTY stream to {destination:?}"))?;
        let mut reader = child
            .stdout
            .take()
            .context("cannot capture PTY stream output")?;
        let mut writer = child.stdin.take().context("cannot open PTY stream input")?;
        writer.write_all(ticket.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        match read_server_frame(&mut reader)? {
            ServerPtyFrame::Ready => Ok(Self {
                reader: Box::new(reader),
                writer: Box::new(writer),
                transport: TransportGuard::Ssh(child),
                cursor,
            }),
            ServerPtyFrame::Error(message) => bail!("remote PTY claim failed: {message}"),
            frame => bail!("remote PTY stream sent {frame:?} before it was ready"),
        }
    }

    pub fn relay(&mut self, escape_prefix: u8) -> Result<DuplexPtyOutcome> {
        let transport_fd = self.reader.as_raw_fd();
        let reader = &mut self.reader;
        let writer = &mut self.writer;
        let cursor = &mut self.cursor;
        xshell_pty::relay_duplex(
            transport_fd,
            escape_prefix,
            |command| match command {
                DuplexPtyCommand::Input(bytes) => {
                    write_client_frame(writer, &ClientPtyFrame::Input(bytes))
                }
                DuplexPtyCommand::Resize(size) => write_client_frame(
                    writer,
                    &ClientPtyFrame::Resize(PtySize {
                        rows: size.rows,
                        columns: size.columns,
                    }),
                ),
            },
            || match read_server_frame(reader)? {
                ServerPtyFrame::Output { offset, bytes } => {
                    *cursor = offset.saturating_add(bytes.len() as u64);
                    Ok(DuplexPtyEvent::Output(bytes))
                }
                ServerPtyFrame::Exit(status) => Ok(DuplexPtyEvent::Exit(status)),
                ServerPtyFrame::Error(message) => Ok(DuplexPtyEvent::Error(message)),
                ServerPtyFrame::Ready => bail!("remote PTY sent a duplicate ready frame"),
                ServerPtyFrame::Detached => bail!("remote PTY detached unexpectedly"),
            },
        )
    }

    pub fn detach(&mut self) -> Result<()> {
        if let Err(error) = write_client_frame(&mut self.writer, &ClientPtyFrame::Close) {
            if transport_is_closed(&error) {
                return Ok(());
            }
            return Err(error);
        }
        loop {
            let frame = match read_server_frame(&mut self.reader) {
                Ok(frame) => frame,
                Err(error) if transport_is_closed(&error) => return Ok(()),
                Err(error) => return Err(error),
            };
            match frame {
                ServerPtyFrame::Detached | ServerPtyFrame::Exit(_) => return Ok(()),
                // Output may already have been in flight when Close was sent.
                // Do not advance the cursor: the next attachment should replay
                // bytes that were never rendered to the controller.
                ServerPtyFrame::Output { .. } => {}
                ServerPtyFrame::Error(message) => bail!("PTY detach failed: {message}"),
                ServerPtyFrame::Ready => bail!("PTY sent a duplicate ready frame while detaching"),
            }
        }
    }

    pub fn cursor(&self) -> u64 {
        self.cursor
    }
}

impl Drop for PtyStreamClient {
    fn drop(&mut self) {
        let _ = write_client_frame(&mut self.writer, &ClientPtyFrame::Close);
        if let TransportGuard::Ssh(child) = &mut self.transport {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub fn write_client_frame(writer: &mut impl Write, frame: &ClientPtyFrame) -> Result<()> {
    match frame {
        ClientPtyFrame::Input(bytes) => write_frame(writer, TAG_INPUT, bytes),
        ClientPtyFrame::Resize(size) => {
            let mut payload = Vec::with_capacity(4);
            payload.extend_from_slice(&size.rows.to_be_bytes());
            payload.extend_from_slice(&size.columns.to_be_bytes());
            write_frame(writer, TAG_RESIZE, &payload)
        }
        ClientPtyFrame::Close => write_frame(writer, TAG_CLOSE, &[]),
    }
}

pub fn read_client_frame(reader: &mut impl Read) -> Result<ClientPtyFrame> {
    let (tag, payload) = read_frame(reader)?;
    match tag {
        TAG_INPUT if payload.len() <= 64 * 1024 => Ok(ClientPtyFrame::Input(payload)),
        TAG_INPUT => bail!("PTY input frame is too large"),
        TAG_RESIZE if payload.len() == 4 => Ok(ClientPtyFrame::Resize(PtySize {
            rows: u16::from_be_bytes([payload[0], payload[1]]),
            columns: u16::from_be_bytes([payload[2], payload[3]]),
        })),
        TAG_CLOSE if payload.is_empty() => Ok(ClientPtyFrame::Close),
        _ => bail!("invalid client PTY frame tag or payload"),
    }
}

pub fn write_server_frame(writer: &mut impl Write, frame: &ServerPtyFrame) -> Result<()> {
    match frame {
        ServerPtyFrame::Ready => write_frame(writer, TAG_READY, &[]),
        ServerPtyFrame::Output { offset, bytes } => {
            let mut payload = Vec::with_capacity(8 + bytes.len());
            payload.extend_from_slice(&offset.to_be_bytes());
            payload.extend_from_slice(bytes);
            write_frame(writer, TAG_OUTPUT, &payload)
        }
        ServerPtyFrame::Exit(status) => write_frame(writer, TAG_EXIT, status.as_bytes()),
        ServerPtyFrame::Error(message) => write_frame(writer, TAG_ERROR, message.as_bytes()),
        ServerPtyFrame::Detached => write_frame(writer, TAG_DETACHED, &[]),
    }
}

pub fn read_server_frame(reader: &mut impl Read) -> Result<ServerPtyFrame> {
    let (tag, payload) = read_frame(reader)?;
    match tag {
        TAG_READY if payload.is_empty() => Ok(ServerPtyFrame::Ready),
        TAG_OUTPUT if payload.len() >= 8 => Ok(ServerPtyFrame::Output {
            offset: u64::from_be_bytes(payload[..8].try_into().unwrap()),
            bytes: payload[8..].to_vec(),
        }),
        TAG_EXIT if payload.len() <= MAX_TEXT_BYTES => Ok(ServerPtyFrame::Exit(text(payload)?)),
        TAG_ERROR if payload.len() <= MAX_TEXT_BYTES => Ok(ServerPtyFrame::Error(text(payload)?)),
        TAG_DETACHED if payload.is_empty() => Ok(ServerPtyFrame::Detached),
        _ => bail!("invalid server PTY frame tag or payload"),
    }
}

fn write_frame(writer: &mut impl Write, tag: u8, payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_FRAME_BYTES {
        bail!("PTY frame exceeds {MAX_FRAME_BYTES} bytes");
    }
    writer.write_all(&[tag])?;
    writer.write_all(&(payload.len() as u32).to_be_bytes())?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

fn read_frame(reader: &mut impl Read) -> Result<(u8, Vec<u8>)> {
    let mut header = [0_u8; 5];
    reader
        .read_exact(&mut header)
        .context("PTY stream closed")?;
    let length = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
    if length > MAX_FRAME_BYTES {
        bail!("PTY frame exceeds {MAX_FRAME_BYTES} bytes");
    }
    let mut payload = vec![0_u8; length];
    reader
        .read_exact(&mut payload)
        .context("truncated PTY frame")?;
    Ok((header[0], payload))
}

fn send_json(writer: &mut impl Write, request: &ClientRequest) -> Result<()> {
    serde_json::to_writer(&mut *writer, request)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn read_json_response(reader: &mut impl Read) -> Result<ServerResponse> {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        reader.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            break;
        }
        if bytes.len() >= MAX_JSON_BYTES {
            bail!("PTY handshake response is too large");
        }
        bytes.push(byte[0]);
    }
    serde_json::from_slice(&bytes).context("invalid PTY handshake response")
}

fn text(payload: Vec<u8>) -> Result<String> {
    String::from_utf8(payload).context("PTY status frame is not UTF-8")
}

fn transport_is_closed(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::UnexpectedEof
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let frames = [
            ClientPtyFrame::Input(vec![0, 1, 255]),
            ClientPtyFrame::Resize(PtySize {
                rows: 42,
                columns: 120,
            }),
            ClientPtyFrame::Close,
        ];
        let mut bytes = Vec::new();
        for frame in &frames {
            write_client_frame(&mut bytes, frame).unwrap();
        }
        let mut input = bytes.as_slice();
        for expected in frames {
            assert_eq!(read_client_frame(&mut input).unwrap(), expected);
        }

        let frame = ServerPtyFrame::Output {
            offset: 17,
            bytes: vec![0, 255],
        };
        let mut bytes = Vec::new();
        write_server_frame(&mut bytes, &frame).unwrap();
        assert_eq!(read_server_frame(&mut bytes.as_slice()).unwrap(), frame);
    }

    #[test]
    fn oversized_frame_is_rejected_before_allocation() {
        let mut bytes = vec![TAG_INPUT];
        bytes.extend_from_slice(&((MAX_FRAME_BYTES + 1) as u32).to_be_bytes());
        assert!(read_client_frame(&mut bytes.as_slice()).is_err());
    }

    #[test]
    fn recognizes_closed_stream_errors_during_detach() {
        let error = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "closed",
        ))
        .context("PTY stream closed");
        assert!(transport_is_closed(&error));
    }
}

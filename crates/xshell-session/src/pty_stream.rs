use crate::PtySize;
use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use xshell_pty::{DuplexPtyCommand, DuplexPtyEvent};

const MAX_FRAME_BYTES: usize = 256 * 1024;
const MAX_TEXT_BYTES: usize = 4 * 1024;
const TAG_INPUT: u8 = 1;
const TAG_RESIZE: u8 = 2;
const TAG_CLOSE: u8 = 3;
const TAG_READY: u8 = 0x80;
const TAG_OUTPUT: u8 = 0x81;
const TAG_EXIT: u8 = 0x82;
const TAG_ERROR: u8 = 0x83;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientPtyFrame {
    Input(Vec<u8>),
    Resize(PtySize),
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerPtyFrame {
    Ready,
    Output(Vec<u8>),
    Exit(String),
    Error(String),
}

pub struct PtyStreamClient {
    reader: ChildStdout,
    writer: ChildStdin,
    child: Child,
}

impl PtyStreamClient {
    pub fn connect_ssh(destination: &str, ticket: &str) -> Result<Self> {
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
        let reader = child
            .stdout
            .take()
            .context("cannot capture PTY stream output")?;
        let mut writer = child.stdin.take().context("cannot open PTY stream input")?;
        writer.write_all(ticket.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        let mut client = Self {
            reader,
            writer,
            child,
        };
        match read_server_frame(&mut client.reader)? {
            ServerPtyFrame::Ready => Ok(client),
            ServerPtyFrame::Error(message) => bail!("remote PTY claim failed: {message}"),
            frame => bail!("remote PTY stream sent {frame:?} before it was ready"),
        }
    }

    pub fn relay(&mut self) -> Result<String> {
        let transport_fd = self.reader.as_raw_fd();
        let reader = &mut self.reader;
        let writer = &mut self.writer;
        xshell_pty::relay_duplex(
            transport_fd,
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
                ServerPtyFrame::Output(bytes) => Ok(DuplexPtyEvent::Output(bytes)),
                ServerPtyFrame::Exit(status) => Ok(DuplexPtyEvent::Exit(status)),
                ServerPtyFrame::Error(message) => Ok(DuplexPtyEvent::Error(message)),
                ServerPtyFrame::Ready => bail!("remote PTY sent a duplicate ready frame"),
            },
        )
    }
}

impl Drop for PtyStreamClient {
    fn drop(&mut self) {
        let _ = write_client_frame(&mut self.writer, &ClientPtyFrame::Close);
        let _ = self.child.kill();
        let _ = self.child.wait();
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
        ServerPtyFrame::Output(bytes) => write_frame(writer, TAG_OUTPUT, bytes),
        ServerPtyFrame::Exit(status) => write_frame(writer, TAG_EXIT, status.as_bytes()),
        ServerPtyFrame::Error(message) => write_frame(writer, TAG_ERROR, message.as_bytes()),
    }
}

pub fn read_server_frame(reader: &mut impl Read) -> Result<ServerPtyFrame> {
    let (tag, payload) = read_frame(reader)?;
    match tag {
        TAG_READY if payload.is_empty() => Ok(ServerPtyFrame::Ready),
        TAG_OUTPUT => Ok(ServerPtyFrame::Output(payload)),
        TAG_EXIT if payload.len() <= MAX_TEXT_BYTES => Ok(ServerPtyFrame::Exit(text(payload)?)),
        TAG_ERROR if payload.len() <= MAX_TEXT_BYTES => Ok(ServerPtyFrame::Error(text(payload)?)),
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

fn text(payload: Vec<u8>) -> Result<String> {
    String::from_utf8(payload).context("PTY status frame is not UTF-8")
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
    }

    #[test]
    fn oversized_frame_is_rejected_before_allocation() {
        let mut bytes = vec![TAG_INPUT];
        bytes.extend_from_slice(&((MAX_FRAME_BYTES + 1) as u32).to_be_bytes());
        assert!(read_client_frame(&mut bytes.as_slice()).is_err());
    }
}

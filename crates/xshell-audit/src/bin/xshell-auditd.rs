use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::Deserialize;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread;
use xshell_audit::{
    AUDIT_PROTOCOL_VERSION, AuditConfig, AuditLogWriter, ClientRequest, ServerResponse,
    SigningIdentity,
};

const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Parser)]
#[command(version, about = "Privileged append-only audit service for xshell")]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,

    #[arg(long)]
    directory: Option<PathBuf>,

    #[arg(long)]
    socket: Option<PathBuf>,

    #[arg(long)]
    checkpoint_interval: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    audit: AuditConfig,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config = load_config(args.config.as_deref())?;
    let directory = args
        .directory
        .or(config.directory)
        .context("audit directory is required (--directory or audit.directory)")?;
    let socket = args
        .socket
        .or(config.socket)
        .context("audit socket is required (--socket or audit.socket)")?;
    let checkpoint_interval = args
        .checkpoint_interval
        .unwrap_or(config.checkpoint_interval);

    let identity = SigningIdentity::load_or_create(&directory)?;
    prepare_socket(&socket)?;
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("cannot bind audit socket {}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o660))?;
    println!("xshell-auditd listening on {}", socket.display());
    println!("audit directory: {}", directory.display());
    println!("signing key ID: {}", identity.key_id());

    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                let directory = directory.clone();
                let identity = identity.clone();
                thread::spawn(move || {
                    if let Err(error) =
                        handle_client(stream, &directory, identity, checkpoint_interval)
                    {
                        eprintln!("xshell-auditd client error: {error:#}");
                    }
                });
            }
            Err(error) => eprintln!("xshell-auditd accept error: {error}"),
        }
    }
    Ok(())
}

fn load_config(path: Option<&Path>) -> Result<AuditConfig> {
    let Some(path) = path else {
        return Ok(AuditConfig::default());
    };
    let source = fs::read_to_string(path)
        .with_context(|| format!("cannot read configuration file {}", path.display()))?;
    let config: ConfigFile = toml::from_str(&source)
        .with_context(|| format!("invalid configuration file {}", path.display()))?;
    Ok(config.audit)
}

fn prepare_socket(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("audit socket must have a parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("cannot create socket directory {}", parent.display()))?;
    if path.exists() {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_socket() {
            bail!("refusing to replace non-socket path {}", path.display());
        }
        fs::remove_file(path)
            .with_context(|| format!("cannot remove stale audit socket {}", path.display()))?;
    }
    Ok(())
}

fn handle_client(
    stream: UnixStream,
    directory: &Path,
    identity: SigningIdentity,
    checkpoint_interval: u64,
) -> Result<()> {
    let client_uid = xshell_platform::peer_uid(&stream)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let Some(mut line) = read_request_line(&mut reader)? else {
        return Ok(());
    };
    let request: ClientRequest = serde_json::from_str(&line).context("invalid open request")?;
    let ClientRequest::Open {
        protocol_version,
        client_version: _,
    } = request
    else {
        send_error(&mut writer, "first request must open a session")?;
        return Ok(());
    };
    if protocol_version != AUDIT_PROTOCOL_VERSION {
        send_error(&mut writer, "unsupported audit protocol version")?;
        return Ok(());
    }

    let mut log = AuditLogWriter::create(directory, identity, client_uid, checkpoint_interval)?;
    send(
        &mut writer,
        &ServerResponse::Opened {
            protocol_version: AUDIT_PROTOCOL_VERSION,
            session_id: log.session_id().to_owned(),
            signing_key_id: log.signing_key_id().to_owned(),
        },
    )?;

    loop {
        let Some(next_line) = read_request_line(&mut reader)? else {
            return Ok(());
        };
        line = next_line;
        let request: ClientRequest = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(_) => {
                send_error(&mut writer, "invalid audit request")?;
                return Ok(());
            }
        };
        match request {
            ClientRequest::Append { event } => match log.append(event) {
                Ok(record) => send(
                    &mut writer,
                    &ServerResponse::Ack {
                        sequence: record.body.sequence,
                        record_hash: record.record_hash,
                    },
                )?,
                Err(error) => {
                    send_error(&mut writer, &format!("durable append failed: {error}"))?;
                    return Err(error);
                }
            },
            ClientRequest::Close => {
                let checkpoint = log.close()?;
                send(&mut writer, &ServerResponse::Closed { checkpoint })?;
                return Ok(());
            }
            ClientRequest::Open { .. } => {
                send_error(&mut writer, "session is already open")?;
                return Ok(());
            }
        }
    }
}

fn read_request_line(reader: &mut BufReader<UnixStream>) -> Result<Option<String>> {
    let mut bytes = Vec::new();
    let count = reader
        .by_ref()
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_until(b'\n', &mut bytes)?;
    if count == 0 {
        return Ok(None);
    }
    if bytes.len() > MAX_REQUEST_BYTES || bytes.last() != Some(&b'\n') {
        bail!("audit request exceeds {MAX_REQUEST_BYTES} bytes");
    }
    String::from_utf8(bytes)
        .map(Some)
        .context("audit request is not valid UTF-8")
}

fn send(stream: &mut UnixStream, response: &ServerResponse) -> Result<()> {
    serde_json::to_writer(&mut *stream, response)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn send_error(stream: &mut UnixStream, message: &str) -> Result<()> {
    send(
        stream,
        &ServerResponse::Error {
            message: message.into(),
        },
    )
}

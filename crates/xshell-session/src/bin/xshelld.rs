use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::Deserialize;
use std::ffi::CStr;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use uuid::Uuid;
use xshell_session::{
    ClientRequest, SESSION_PROTOCOL_VERSION, ServerResponse, SessionConfig, SessionRegistry,
};

const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Local session registry and persistence service for xshell"
)]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,

    #[arg(long)]
    state_directory: Option<PathBuf>,

    #[arg(long)]
    socket: Option<PathBuf>,

    #[arg(long)]
    host_alias: Option<String>,

    #[arg(long)]
    user: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    session_fabric: SessionConfig,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config = load_config(args.config.as_deref())?;
    let state_directory = args
        .state_directory
        .or_else(|| config.resolved_state_directory())
        .context("session state directory is required")?;
    let socket = args
        .socket
        .or(config.socket)
        .unwrap_or_else(|| state_directory.join("xshelld.sock"));
    let host_alias = args.host_alias.unwrap_or_else(system_hostname);
    let user = args.user.unwrap_or_else(system_user);
    let host_id = load_or_create_host_id(&state_directory)?;
    let registry = Arc::new(Mutex::new(SessionRegistry::load(
        state_directory.clone(),
        host_id,
        host_alias,
        user,
    )?));

    prepare_socket(&socket)?;
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("cannot bind session socket {}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let identity = registry.lock().expect("session registry poisoned");
    println!("xshelld listening on {}", socket.display());
    println!(
        "host: {} ({}) user: {}",
        identity.host_alias(),
        identity.host_id(),
        identity.user()
    );
    drop(identity);

    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                let registry = Arc::clone(&registry);
                thread::spawn(move || {
                    if let Err(error) = handle_client(stream, registry) {
                        eprintln!("xshelld client error: {error:#}");
                    }
                });
            }
            Err(error) => eprintln!("xshelld accept error: {error}"),
        }
    }
    Ok(())
}

fn load_config(path: Option<&Path>) -> Result<SessionConfig> {
    let Some(path) = path else {
        return Ok(SessionConfig::default());
    };
    let source = fs::read_to_string(path)
        .with_context(|| format!("cannot read configuration file {}", path.display()))?;
    let config: ConfigFile = toml::from_str(&source)
        .with_context(|| format!("invalid configuration file {}", path.display()))?;
    Ok(config.session_fabric)
}

fn handle_client(stream: UnixStream, registry: Arc<Mutex<SessionRegistry>>) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let Some(line) = read_request_line(&mut reader)? else {
        return Ok(());
    };
    let request: ClientRequest = serde_json::from_str(&line).context("invalid open request")?;
    let ClientRequest::Open {
        protocol_version,
        client_version: _,
    } = request
    else {
        send_error(
            &mut writer,
            "protocol",
            "first request must open a connection",
        )?;
        return Ok(());
    };
    if protocol_version != SESSION_PROTOCOL_VERSION {
        send_error(
            &mut writer,
            "protocol_version",
            "unsupported session protocol version",
        )?;
        return Ok(());
    }

    let client_id = Uuid::new_v4().to_string();
    {
        let registry = registry.lock().expect("session registry poisoned");
        send(
            &mut writer,
            &ServerResponse::Opened {
                protocol_version: SESSION_PROTOCOL_VERSION,
                client_id: client_id.clone(),
                host_id: registry.host_id().to_owned(),
                host_alias: registry.host_alias().to_owned(),
                user: registry.user().to_owned(),
            },
        )?;
    }

    let mut attached_session: Option<String> = None;
    loop {
        let Some(line) = read_request_line(&mut reader)? else {
            detach_on_disconnect(&registry, &client_id, attached_session.as_deref());
            return Ok(());
        };
        let request: ClientRequest = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                send_error(&mut writer, "invalid_request", &error.to_string())?;
                continue;
            }
        };
        let response = process_request(request, &registry, &client_id, &mut attached_session);
        match response {
            Ok(response) => send(&mut writer, &response)?,
            Err(error) => send_error(&mut writer, "request_failed", &format!("{error:#}"))?,
        }
    }
}

fn process_request(
    request: ClientRequest,
    registry: &Arc<Mutex<SessionRegistry>>,
    client_id: &str,
    attached_session: &mut Option<String>,
) -> Result<ServerResponse> {
    let mut registry = registry.lock().expect("session registry poisoned");
    match request {
        ClientRequest::List => Ok(ServerResponse::Catalog {
            sessions: registry.list(),
        }),
        ClientRequest::Create { session: creation } => {
            let session = registry.create(client_id, creation)?;
            if let Some(previous) = attached_session.take() {
                registry.detach(client_id, &previous)?;
            }
            *attached_session = Some(session.descriptor.id.clone());
            Ok(ServerResponse::Created { session })
        }
        ClientRequest::Attach { selector, role } => {
            if attached_session.is_some() {
                bail!("detach the current session before attaching another one");
            }
            let session = registry.attach(client_id, &selector, role)?;
            *attached_session = Some(session.descriptor.id.clone());
            Ok(ServerResponse::Attached { session, role })
        }
        ClientRequest::Switch { selector, role } => {
            let session = registry.attach(client_id, &selector, role)?;
            if let Some(previous) = attached_session.take()
                && previous != session.descriptor.id
            {
                registry.detach(client_id, &previous)?;
            }
            *attached_session = Some(session.descriptor.id.clone());
            Ok(ServerResponse::Attached { session, role })
        }
        ClientRequest::Update {
            session_id,
            model,
            cwd,
            history,
        } => {
            if attached_session.as_deref() != Some(session_id.as_str()) {
                bail!("the requested session is not this connection's current session");
            }
            let session = registry.update(client_id, &session_id, model, cwd, history)?;
            Ok(ServerResponse::Updated { session })
        }
        ClientRequest::Detach => {
            let detached = match attached_session.take() {
                Some(session_id) => registry.detach(client_id, &session_id)?,
                None => None,
            };
            Ok(ServerResponse::Detached {
                session_id: detached,
            })
        }
        ClientRequest::Close { selector } => {
            let selector = selector
                .or_else(|| attached_session.clone())
                .context("close requires a session selector when detached")?;
            let session_id = registry.close(client_id, &selector)?;
            if attached_session.as_deref() == Some(session_id.as_str()) {
                *attached_session = None;
            }
            Ok(ServerResponse::Closed { session_id })
        }
        ClientRequest::Open { .. } => bail!("connection is already open"),
    }
}

fn detach_on_disconnect(
    registry: &Arc<Mutex<SessionRegistry>>,
    client_id: &str,
    session_id: Option<&str>,
) {
    if let Some(session_id) = session_id
        && let Err(error) = registry
            .lock()
            .expect("session registry poisoned")
            .detach(client_id, session_id)
    {
        eprintln!("xshelld detach error: {error:#}");
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
        bail!("session request exceeds {MAX_REQUEST_BYTES} bytes");
    }
    Ok(Some(
        String::from_utf8(bytes).context("session request is not UTF-8")?,
    ))
}

fn send(writer: &mut UnixStream, response: &ServerResponse) -> Result<()> {
    serde_json::to_writer(&mut *writer, response)?;
    writer.write_all(b"\n")?;
    writer.flush().context("cannot flush session response")
}

fn send_error(writer: &mut UnixStream, code: &str, message: &str) -> Result<()> {
    send(
        writer,
        &ServerResponse::Error {
            code: code.to_owned(),
            message: message.to_owned(),
        },
    )
}

fn prepare_socket(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("session socket must have a parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("cannot create socket directory {}", parent.display()))?;
    if path.exists() {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_socket() {
            bail!("refusing to replace non-socket path {}", path.display());
        }
        fs::remove_file(path)
            .with_context(|| format!("cannot remove stale session socket {}", path.display()))?;
    }
    Ok(())
}

fn load_or_create_host_id(state_directory: &Path) -> Result<String> {
    fs::create_dir_all(state_directory)?;
    fs::set_permissions(state_directory, fs::Permissions::from_mode(0o700))?;
    let path = state_directory.join("host-id");
    if path.exists() {
        let value = fs::read_to_string(&path)?;
        let value = value.trim();
        Uuid::parse_str(value).context("invalid host ID")?;
        return Ok(value.to_owned());
    }
    let value = Uuid::new_v4().to_string();
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options.open(&path)?;
    writeln!(file, "{value}")?;
    file.sync_all()?;
    Ok(value)
}

fn system_hostname() -> String {
    let mut buffer = [0_i8; 256];
    if unsafe { libc::gethostname(buffer.as_mut_ptr(), buffer.len() - 1) } == 0 {
        let hostname = unsafe { CStr::from_ptr(buffer.as_ptr()) };
        if let Ok(hostname) = hostname.to_str() {
            return hostname.to_owned();
        }
    }
    "localhost".to_owned()
}

fn system_user() -> String {
    std::env::var("USER").unwrap_or_else(|_| unsafe { libc::geteuid() }.to_string())
}

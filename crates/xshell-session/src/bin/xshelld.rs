use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::ffi::CStr;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;
use uuid::Uuid;
use xshell_session::{
    ClientPtyFrame, ClientRequest, DaemonAudit, ExecutionCoordinator, PersistenceMode, PtyClaim,
    PtyCoordinator, SESSION_PROTOCOL_VERSION, ServerPtyFrame, ServerResponse, SessionActivity,
    SessionConfig, SessionRegistry, complete_shell, load_view_resource, read_client_frame,
    write_server_frame,
};

const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(1);
const VIEW_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Local session execution and persistence service for xshell"
)]
struct Args {
    #[command(subcommand)]
    command: Option<DaemonCommand>,

    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Ignore XSHELL_CONFIG and ~/.config/xshell/config.toml; use only
    /// command-line flags and built-in defaults. Intended for tests and
    /// hermetic deployments.
    #[arg(long, global = true, conflicts_with = "config")]
    no_user_config: bool,

    #[arg(long, global = true)]
    state_directory: Option<PathBuf>,

    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    #[arg(long, global = true)]
    host_alias: Option<String>,

    #[arg(long, global = true)]
    user: Option<String>,
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Proxy the session protocol between stdin/stdout and the local daemon.
    ServeStdio,
    /// Claim a PTY ticket and proxy its framed binary stream over stdin/stdout.
    ServePtyStdio,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    session_fabric: SessionConfig,
    #[serde(default)]
    audit: xshell_audit::AuditConfig,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config_path = if args.no_user_config {
        None
    } else {
        resolve_config_path(args.config)?
    };
    let (config, audit_config) = load_config(config_path.as_deref())?;
    let state_directory = args
        .state_directory
        .or_else(|| config.resolved_state_directory())
        .context("session state directory is required")?;
    let socket = args
        .socket
        .or(config.socket)
        .unwrap_or_else(|| state_directory.join("xshelld.sock"));
    if matches!(args.command, Some(DaemonCommand::ServeStdio)) {
        return serve_stdio(&socket);
    }
    if matches!(args.command, Some(DaemonCommand::ServePtyStdio)) {
        return serve_pty_stdio(&socket);
    }
    let host_alias = args.host_alias.unwrap_or_else(system_hostname);
    let user = args.user.unwrap_or_else(system_user);
    let host_id = load_or_create_host_id(&state_directory)?;
    let registry = Arc::new(Mutex::new(SessionRegistry::load(
        state_directory.clone(),
        host_id,
        host_alias,
        user,
    )?));
    let audit = DaemonAudit::from_config(&audit_config)?;
    let execution =
        ExecutionCoordinator::with_policy(Arc::clone(&registry), audit, config.max_approval);
    let ptys = PtyCoordinator::default();

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
    println!("max approval: {}", execution.max_approval());
    println!(
        "audit: {}",
        match (execution.audit().enabled(), execution.audit().required()) {
            (false, _) => "disabled".to_owned(),
            (true, true) => "required (execution-boundary)".to_owned(),
            (true, false) => "best-effort (execution-boundary)".to_owned(),
        }
    );

    install_shutdown_handler(execution.audit().clone());

    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                // The daemon runs commands as the invoking user. Socket mode
                // is not a sufficient control on every platform, so verify
                // the peer explicitly before reading a single request byte.
                if let Err(error) = xshell_platform::require_same_user(&stream, "session") {
                    eprintln!("xshelld: {error:#}");
                    continue;
                }
                let registry = Arc::clone(&registry);
                let execution = execution.clone();
                let ptys = ptys.clone();
                thread::spawn(move || {
                    if let Err(error) = handle_client(stream, registry, execution, ptys) {
                        eprintln!("xshelld client error: {error:#}");
                    }
                });
            }
            Err(error) => eprintln!("xshelld accept error: {error}"),
        }
    }
    Ok(())
}

/// On SIGINT/SIGTERM, finalize open audit sessions with signed checkpoints and
/// exit. Without this, `launchctl stop` or Ctrl-C would leave every daemon
/// audit log without a final checkpoint, which the verifier reports as
/// possibly truncated.
fn install_shutdown_handler(audit: DaemonAudit) {
    thread::Builder::new()
        .name("xshelld-shutdown".into())
        .spawn(move || {
            let mut signals = match signal_hook::iterator::Signals::new([
                signal_hook::consts::SIGINT,
                signal_hook::consts::SIGTERM,
            ]) {
                Ok(signals) => signals,
                Err(error) => {
                    eprintln!("xshelld: cannot install shutdown handler: {error}");
                    return;
                }
            };
            if signals.forever().next().is_some() {
                audit.close_all("xshelld shutdown");
                std::process::exit(0);
            }
        })
        .expect("cannot spawn shutdown handler thread");
}

fn resolve_config_path(explicit: Option<PathBuf>) -> Result<Option<PathBuf>> {
    if explicit.is_some() {
        return Ok(explicit);
    }
    if let Some(path) = std::env::var_os("XSHELL_CONFIG") {
        return Ok(Some(PathBuf::from(path)));
    }
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(None);
    };
    let path = PathBuf::from(home).join(".config/xshell/config.toml");
    Ok(path.exists().then_some(path))
}

fn serve_stdio(socket: &Path) -> Result<()> {
    let stream = UnixStream::connect(socket).with_context(|| {
        format!(
            "cannot connect stdio transport to xshell session service at {}",
            socket.display()
        )
    })?;
    let mut daemon_reader = BufReader::new(stream.try_clone()?);
    let mut daemon_writer = stream;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut client_reader = BufReader::new(stdin.lock());
    let mut client_writer = stdout.lock();

    while let Some(line) = read_request_line(&mut client_reader)? {
        let request: ClientRequest =
            serde_json::from_str(&line).context("invalid stdio request")?;
        if let Some(response) =
            reject_remote_request(&request, &mut daemon_reader, &mut daemon_writer)?
        {
            send(&mut client_writer, &response)?;
            continue;
        }

        serde_json::to_writer(&mut daemon_writer, &request)?;
        daemon_writer.write_all(b"\n")?;
        daemon_writer.flush()?;
        let response_line = read_request_line(&mut daemon_reader)?
            .context("local session daemon closed the stdio proxy connection")?;
        let mut response: ServerResponse =
            serde_json::from_str(&response_line).context("invalid local daemon response")?;
        if let ServerResponse::Catalog { sessions } = &mut response {
            sessions.retain(|session| session.visibility == xshell_session::Visibility::Fabric);
        }
        if let ServerResponse::PtyCatalog { ptys } = &mut response {
            serde_json::to_writer(&mut daemon_writer, &ClientRequest::List)?;
            daemon_writer.write_all(b"\n")?;
            daemon_writer.flush()?;
            let line = read_request_line(&mut daemon_reader)?
                .context("local daemon closed while filtering terminal jobs")?;
            let catalog: ServerResponse = serde_json::from_str(&line)?;
            let ServerResponse::Catalog { sessions } = catalog else {
                bail!("local daemon returned an invalid terminal visibility catalog");
            };
            ptys.retain(|pty| {
                sessions.iter().any(|session| {
                    session.id == pty.session_id
                        && session.visibility == xshell_session::Visibility::Fabric
                })
            });
        }
        send(&mut client_writer, &response)?;
    }
    Ok(())
}

fn serve_pty_stdio(socket: &Path) -> Result<()> {
    let stream = UnixStream::connect(socket).with_context(|| {
        format!(
            "cannot connect PTY transport to xshell session service at {}",
            socket.display()
        )
    })?;
    let mut daemon_reader = BufReader::new(stream.try_clone()?);
    let mut daemon_writer = stream;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut client_reader = BufReader::new(stdin);
    let mut client_writer = stdout.lock();

    let mut ticket = String::new();
    client_reader
        .read_line(&mut ticket)
        .context("cannot read PTY stream ticket")?;
    let ticket = ticket.trim_end_matches(['\r', '\n']);
    if ticket.is_empty() || ticket.len() > 128 {
        bail!("invalid PTY stream ticket");
    }
    send_request(
        &mut daemon_writer,
        &ClientRequest::open(env!("CARGO_PKG_VERSION")),
    )?;
    match receive_response(&mut daemon_reader)? {
        ServerResponse::Opened { .. } => {}
        response => bail!("PTY proxy could not open daemon connection: {response:?}"),
    }
    send_request(
        &mut daemon_writer,
        &ClientRequest::PtyClaim {
            ticket: ticket.to_owned(),
        },
    )?;
    match receive_response(&mut daemon_reader)? {
        ServerResponse::PtyClaimed => {
            write_server_frame(&mut client_writer, &ServerPtyFrame::Ready)?;
        }
        ServerResponse::Error { message, .. } => {
            write_server_frame(&mut client_writer, &ServerPtyFrame::Error(message))?;
            return Ok(());
        }
        response => bail!("unexpected PTY claim response: {response:?}"),
    }

    let upload = thread::spawn(move || std::io::copy(&mut client_reader, &mut daemon_writer));
    copy_and_flush(&mut daemon_reader, &mut client_writer)?;
    drop(client_writer);
    let _ = upload.join();
    Ok(())
}

fn copy_and_flush(reader: &mut impl Read, writer: &mut impl Write) -> Result<()> {
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        writer.write_all(&buffer[..count])?;
        writer.flush()?;
    }
}

fn receive_response(reader: &mut impl BufRead) -> Result<ServerResponse> {
    let line = read_request_line(reader)?.context("session daemon closed the connection")?;
    serde_json::from_str(&line).context("invalid session daemon response")
}

fn reject_remote_request(
    request: &ClientRequest,
    daemon_reader: &mut BufReader<UnixStream>,
    daemon_writer: &mut UnixStream,
) -> Result<Option<ServerResponse>> {
    if matches!(request, ClientRequest::PtyClaim { .. }) {
        return Ok(Some(ServerResponse::Error {
            code: "dedicated_pty_connection_required".into(),
            message: "PTY tickets may only be claimed through serve-pty-stdio".into(),
        }));
    }
    if matches!(
        request,
        ClientRequest::Create { session }
            if session.visibility == xshell_session::Visibility::HostOnly
    ) {
        return Ok(Some(remote_visibility_error()));
    }
    let selector = match request {
        ClientRequest::Attach { selector, .. } | ClientRequest::Switch { selector, .. } => {
            Some(selector.as_str())
        }
        ClientRequest::Close {
            selector: Some(selector),
        } => Some(selector.as_str()),
        ClientRequest::CompleteShell { session_id, .. } => Some(session_id.as_str()),
        ClientRequest::ViewSource { session_id, .. } => Some(session_id.as_str()),
        ClientRequest::PtyStart { session_id, .. } => Some(session_id.as_str()),
        ClientRequest::PtyAttach { session_id, .. } => Some(session_id.as_str()),
        _ => None,
    };
    let Some(selector) = selector else {
        return Ok(None);
    };

    serde_json::to_writer(&mut *daemon_writer, &ClientRequest::List)?;
    daemon_writer.write_all(b"\n")?;
    daemon_writer.flush()?;
    let line = read_request_line(daemon_reader)?
        .context("local session daemon closed during visibility check")?;
    let response: ServerResponse = serde_json::from_str(&line)?;
    let visible = matches!(
        response,
        ServerResponse::Catalog { sessions }
            if sessions.iter().any(|session| {
                session.visibility == xshell_session::Visibility::Fabric
                    && (session.id == selector || session.name == selector)
            })
    );
    Ok((!visible).then(remote_visibility_error))
}

fn remote_visibility_error() -> ServerResponse {
    ServerResponse::Error {
        code: "remote_session_not_visible".into(),
        message: "session is unavailable through the remote transport".into(),
    }
}

fn load_config(path: Option<&Path>) -> Result<(SessionConfig, xshell_audit::AuditConfig)> {
    let Some(path) = path else {
        return Ok((
            SessionConfig::default(),
            xshell_audit::AuditConfig::default(),
        ));
    };
    let source = fs::read_to_string(path)
        .with_context(|| format!("cannot read configuration file {}", path.display()))?;
    let config: ConfigFile = toml::from_str(&source)
        .with_context(|| format!("invalid configuration file {}", path.display()))?;
    Ok((config.session_fabric, config.audit))
}

fn handle_client(
    stream: UnixStream,
    registry: Arc<Mutex<SessionRegistry>>,
    execution: ExecutionCoordinator,
    ptys: PtyCoordinator,
) -> Result<()> {
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
    let result = loop {
        let line = match read_request_line(&mut reader) {
            Ok(line) => line,
            Err(error) => break Err(error),
        };
        let Some(line) = line else {
            break Ok(());
        };
        let request: ClientRequest = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                if let Err(error) = send_error(&mut writer, "invalid_request", &error.to_string()) {
                    break Err(error);
                }
                continue;
            }
        };
        if let ClientRequest::PtyClaim { ticket } = request {
            let claim = match ptys.claim(&ticket) {
                Ok(claim) => claim,
                Err(error) => {
                    if let Err(error) =
                        send_error(&mut writer, "pty_claim_failed", &format!("{error:#}"))
                    {
                        break Err(error);
                    }
                    continue;
                }
            };
            if let Err(error) = send(&mut writer, &ServerResponse::PtyClaimed) {
                ptys.release_claim(&claim);
                break Err(error);
            }
            let served = serve_claimed_pty(&mut reader, &mut writer, &ptys, &claim);
            ptys.release_claim(&claim);
            break served;
        }
        let response = process_request(
            request,
            &registry,
            &execution,
            &ptys,
            &client_id,
            &mut attached_session,
        );
        let sent = match response {
            Ok(response) => send(&mut writer, &response),
            Err(error) => send_error(&mut writer, "request_failed", &format!("{error:#}")),
        };
        if let Err(error) = sent {
            break Err(error);
        }
    };
    detach_on_disconnect(
        &registry,
        &execution,
        &ptys,
        &client_id,
        attached_session.as_deref(),
    );
    result
}

fn serve_claimed_pty(
    reader: &mut BufReader<UnixStream>,
    writer: &mut UnixStream,
    ptys: &PtyCoordinator,
    claim: &PtyClaim,
) -> Result<()> {
    let mut cursor = claim.cursor;
    loop {
        let mut descriptor = libc::pollfd {
            fd: reader.get_ref().as_raw_fd(),
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        let polled = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if polled < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).context("PTY stream poll failed");
            }
        } else if !reader.buffer().is_empty() || descriptor.revents & libc::POLLIN != 0 {
            match read_client_frame(reader)? {
                ClientPtyFrame::Input(bytes) => {
                    ptys.write_claimed(claim, bytes)?;
                }
                ClientPtyFrame::Resize(updated) => ptys.resize_claimed(claim, updated)?,
                ClientPtyFrame::Close => {
                    ptys.release_claim(claim);
                    write_server_frame(writer, &ServerPtyFrame::Detached)?;
                    return Ok(());
                }
            }
        } else if descriptor.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return Ok(());
        }

        let result = match ptys.read_claimed(claim, cursor, 40) {
            Ok(result) => result,
            Err(error) => {
                let message = format!("{error:#}");
                let _ = write_server_frame(writer, &ServerPtyFrame::Error(message));
                return Ok(());
            }
        };
        cursor = result.offset.saturating_add(result.output.len() as u64);
        if !result.output.is_empty() {
            write_server_frame(
                writer,
                &ServerPtyFrame::Output {
                    offset: result.offset,
                    bytes: result.output,
                },
            )?;
        }
        if let Some(status) = result.status {
            write_server_frame(writer, &ServerPtyFrame::Exit(status))?;
            return Ok(());
        }
    }
}

fn process_request(
    request: ClientRequest,
    registry: &Arc<Mutex<SessionRegistry>>,
    execution: &ExecutionCoordinator,
    ptys: &PtyCoordinator,
    client_id: &str,
    attached_session: &mut Option<String>,
) -> Result<ServerResponse> {
    match request {
        ClientRequest::List => {
            let mut sessions = registry.lock().expect("session registry poisoned").list();
            for session in &mut sessions {
                session.activity = session_activity(execution, ptys, &session.id);
            }
            Ok(ServerResponse::Catalog { sessions })
        }
        ClientRequest::Create {
            session: mut creation,
        } => {
            if creation.cwd == Path::new("~") {
                let home = std::env::var_os("HOME").context("HOME is not set on session host")?;
                creation.cwd = PathBuf::from(home)
                    .canonicalize()
                    .context("cannot resolve session host home directory")?;
            }
            let session = registry
                .lock()
                .expect("session registry poisoned")
                .create(client_id, creation)?;
            if let Some(previous) = attached_session.take() {
                detach_session(registry, execution, ptys, client_id, &previous)?;
            }
            *attached_session = Some(session.descriptor.id.clone());
            Ok(ServerResponse::Created {
                session: with_activity(session, execution, ptys),
            })
        }
        ClientRequest::Attach { selector, role } => {
            if attached_session.is_some() {
                bail!("detach the current session before attaching another one");
            }
            let session = registry
                .lock()
                .expect("session registry poisoned")
                .attach(client_id, &selector, role)?;
            *attached_session = Some(session.descriptor.id.clone());
            Ok(ServerResponse::Attached {
                session: with_activity(session, execution, ptys),
                role,
            })
        }
        ClientRequest::Switch { selector, role } => {
            let session = registry
                .lock()
                .expect("session registry poisoned")
                .attach(client_id, &selector, role)?;
            if let Some(previous) = attached_session.take()
                && previous != session.descriptor.id
            {
                detach_session(registry, execution, ptys, client_id, &previous)?;
            }
            *attached_session = Some(session.descriptor.id.clone());
            Ok(ServerResponse::Attached {
                session: with_activity(session, execution, ptys),
                role,
            })
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
            if execution.active_turn(&session_id).is_some() {
                bail!("cannot replace session state while a turn is active");
            }
            if ptys.has_session(&session_id) {
                let snapshot = registry
                    .lock()
                    .expect("session registry poisoned")
                    .snapshot(&session_id)?;
                if snapshot.descriptor.model == model
                    && snapshot.descriptor.cwd == cwd
                    && snapshot.history == history
                {
                    return Ok(ServerResponse::Updated {
                        session: descriptor_with_activity(snapshot.descriptor, execution, ptys),
                    });
                }
                bail!("cannot replace session state while a PTY is active");
            }
            let session = registry.lock().expect("session registry poisoned").update(
                client_id,
                &session_id,
                model,
                cwd,
                history,
            )?;
            Ok(ServerResponse::Updated {
                session: descriptor_with_activity(session, execution, ptys),
            })
        }
        ClientRequest::Snapshot { session_id } => {
            require_current(attached_session, &session_id)?;
            let session = registry
                .lock()
                .expect("session registry poisoned")
                .snapshot(&session_id)?;
            Ok(ServerResponse::Snapshot {
                session: with_activity(session, execution, ptys),
            })
        }
        ClientRequest::Submit {
            session_id,
            input,
            approval,
        } => {
            require_current(attached_session, &session_id)?;
            if ptys.has_session(&session_id) {
                bail!("cannot start a turn while a PTY is active");
            }
            let turn_id = execution.submit(&session_id, input, approval)?;
            Ok(ServerResponse::Accepted { turn_id })
        }
        ClientRequest::Events {
            session_id,
            after_sequence,
            wait_ms,
        } => {
            require_current(attached_session, &session_id)?;
            Ok(ServerResponse::Events {
                batch: execution.events(&session_id, after_sequence, wait_ms),
            })
        }
        ClientRequest::Approve { session_id, reply } => {
            require_current(attached_session, &session_id)?;
            execution.approve(&session_id, reply)?;
            Ok(ServerResponse::ApprovalAccepted)
        }
        ClientRequest::Cancel {
            session_id,
            turn_id,
        } => {
            require_current(attached_session, &session_id)?;
            execution.cancel(&session_id, &turn_id)?;
            Ok(ServerResponse::CancellationAccepted)
        }
        ClientRequest::CompleteShell {
            session_id,
            line,
            cursor,
        } => {
            let cwd = registry
                .lock()
                .expect("session registry poisoned")
                .snapshot(&session_id)?
                .descriptor
                .cwd;
            Ok(ServerResponse::ShellCompletions {
                result: complete_shell_bounded(line, cursor, cwd)?,
            })
        }
        ClientRequest::ViewSource { session_id, path } => {
            require_current(attached_session, &session_id)?;
            let cwd = registry
                .lock()
                .expect("session registry poisoned")
                .snapshot(&session_id)?
                .descriptor
                .cwd;
            Ok(ServerResponse::ViewSource {
                resource: load_view_resource_bounded(path, cwd)?,
            })
        }
        ClientRequest::PtyStart {
            session_id,
            command,
            size,
            terminal_type,
        } => {
            require_current(attached_session, &session_id)?;
            if execution.active_turn(&session_id).is_some() {
                bail!("cannot start a PTY while a turn is active");
            }
            let cwd = registry
                .lock()
                .expect("session registry poisoned")
                .snapshot(&session_id)?
                .descriptor
                .cwd;
            let ticket = ptys.start(&session_id, command, &cwd, size, terminal_type)?;
            Ok(ServerResponse::PtyStarted { ticket })
        }
        ClientRequest::PtyList => Ok(ServerResponse::PtyCatalog { ptys: ptys.list() }),
        ClientRequest::PtyAttach {
            session_id,
            after_offset,
        } => {
            require_current(attached_session, &session_id)?;
            Ok(ServerResponse::PtyAttached {
                ticket: ptys.attach(&session_id, after_offset)?,
            })
        }
        ClientRequest::PtyClose { pty_id } => {
            let session_id = ptys.session_id(&pty_id)?;
            require_current(attached_session, &session_id)?;
            ptys.terminate(&pty_id)?;
            Ok(ServerResponse::PtyClosed)
        }
        ClientRequest::PtyClaim { .. } => bail!("PTY claims require a dedicated connection"),
        ClientRequest::Detach => {
            let detached = match attached_session.take() {
                Some(session_id) => {
                    detach_session(registry, execution, ptys, client_id, &session_id)?
                }
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
            let resolved = registry
                .lock()
                .expect("session registry poisoned")
                .snapshot(&selector)?
                .descriptor
                .id;
            ptys.terminate_session(&resolved);
            let session_id = registry
                .lock()
                .expect("session registry poisoned")
                .close(client_id, &selector)?;
            execution.cancel_and_remove(&resolved);
            execution.audit().close_session(&resolved, "session closed");
            if attached_session.as_deref() == Some(session_id.as_str()) {
                *attached_session = None;
            }
            Ok(ServerResponse::Closed { session_id })
        }
        ClientRequest::Open { .. } => bail!("connection is already open"),
    }
}

fn complete_shell_bounded(
    line: String,
    cursor: usize,
    cwd: PathBuf,
) -> Result<xshell_session::ShellCompletionResult> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("xshelld-completion".into())
        .spawn(move || {
            let _ = sender.send(complete_shell(&line, cursor, &cwd));
        })
        .context("cannot start shell completion worker")?;
    receiver
        .recv_timeout(COMPLETION_TIMEOUT)
        .context("shell completion timed out")?
}

fn load_view_resource_bounded(path: PathBuf, cwd: PathBuf) -> Result<xshell_session::ViewResource> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("xshelld-view-source".into())
        .spawn(move || {
            let _ = sender.send(load_view_resource(&path, &cwd));
        })
        .context("cannot start view-source worker")?;
    receiver
        .recv_timeout(VIEW_TIMEOUT)
        .context("view-source acquisition timed out")?
}

fn require_current(attached_session: &Option<String>, session_id: &str) -> Result<()> {
    if attached_session.as_deref() != Some(session_id) {
        bail!("the requested session is not this connection's current session");
    }
    Ok(())
}

fn with_activity(
    mut snapshot: xshell_session::SessionSnapshot,
    execution: &ExecutionCoordinator,
    ptys: &PtyCoordinator,
) -> xshell_session::SessionSnapshot {
    snapshot.descriptor.activity = session_activity(execution, ptys, &snapshot.descriptor.id);
    snapshot
}

fn descriptor_with_activity(
    mut descriptor: xshell_session::SessionDescriptor,
    execution: &ExecutionCoordinator,
    ptys: &PtyCoordinator,
) -> xshell_session::SessionDescriptor {
    descriptor.activity = session_activity(execution, ptys, &descriptor.id);
    descriptor
}

fn session_activity(
    execution: &ExecutionCoordinator,
    ptys: &PtyCoordinator,
    session_id: &str,
) -> SessionActivity {
    if ptys.has_session(session_id) {
        SessionActivity::Running
    } else {
        execution.activity(session_id)
    }
}

fn detach_on_disconnect(
    registry: &Arc<Mutex<SessionRegistry>>,
    execution: &ExecutionCoordinator,
    ptys: &PtyCoordinator,
    client_id: &str,
    session_id: Option<&str>,
) {
    if let Some(session_id) = session_id
        && let Err(error) = detach_session(registry, execution, ptys, client_id, session_id)
    {
        eprintln!("xshelld detach error: {error:#}");
    }
}

fn detach_session(
    registry: &Arc<Mutex<SessionRegistry>>,
    execution: &ExecutionCoordinator,
    ptys: &PtyCoordinator,
    client_id: &str,
    session_id: &str,
) -> Result<Option<String>> {
    let persistence = registry
        .lock()
        .expect("session registry poisoned")
        .snapshot(session_id)?
        .descriptor
        .persistence;
    if persistence == PersistenceMode::Ephemeral {
        execution.cancel_and_remove(session_id);
        ptys.terminate_session(session_id);
    }
    registry
        .lock()
        .expect("session registry poisoned")
        .detach(client_id, session_id)
}

fn read_request_line(reader: &mut impl BufRead) -> Result<Option<String>> {
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

fn send(writer: &mut impl Write, response: &ServerResponse) -> Result<()> {
    serde_json::to_writer(&mut *writer, response)?;
    writer.write_all(b"\n")?;
    writer.flush().context("cannot flush session response")
}

fn send_request(writer: &mut impl Write, request: &ClientRequest) -> Result<()> {
    serde_json::to_writer(&mut *writer, request)?;
    writer.write_all(b"\n")?;
    writer.flush().context("cannot flush session request")
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
    // The socket's parent must be ours and private; otherwise another local
    // user could pre-create the directory (for example under /tmp) and
    // replace or redirect the socket.
    xshell_platform::ensure_secure_directory(parent, "session socket")?;
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
    xshell_platform::ensure_secure_directory(state_directory, "session state")?;
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
    // `c_char` is `i8` on x86_64 but `u8` on aarch64 Linux; spell the element
    // type through libc so this compiles on every supported target.
    let mut buffer = [0 as libc::c_char; 256];
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

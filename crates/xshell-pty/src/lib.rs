use anyhow::{Context, Result, bail};
use nix::pty::{OpenptyResult, Winsize, openpty};
use nix::sys::termios::{SetArg, Termios, cfmakeraw, tcgetattr, tcsetattr};
use std::env;
use std::fs::File;
use std::io::{self, IsTerminal, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Duration;

const RELAY_BUFFER_BYTES: usize = 16 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const REMOTE_INPUT_BYTES: usize = 16 * 1024;
const REMOTE_WAIT: Duration = Duration::from_millis(40);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub rows: u16,
    pub columns: u16,
}

impl Default for PtySize {
    fn default() -> Self {
        Self {
            rows: 24,
            columns: 80,
        }
    }
}

pub fn parse_escape_prefix(value: &str) -> Result<u8> {
    let value = value.trim().to_ascii_lowercase();
    if let Some(key) = value.strip_prefix("ctrl-") {
        let bytes = key.as_bytes();
        if bytes.len() == 1 && (b'@'..=b'_').contains(&bytes[0].to_ascii_uppercase()) {
            return Ok(bytes[0].to_ascii_uppercase() & 0x1f);
        }
    }
    let bytes = value.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_graphic() {
        return Ok(bytes[0]);
    }
    bail!("PTY escape must be a single ASCII key or ctrl-KEY (for example ctrl-])")
}

impl From<PtySize> for Winsize {
    fn from(size: PtySize) -> Self {
        Self {
            ws_row: size.rows,
            ws_col: size.columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }
    }
}

impl From<Winsize> for PtySize {
    fn from(size: Winsize) -> Self {
        Self {
            rows: size.ws_row,
            columns: size.ws_col,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePtyChunk {
    pub output: Vec<u8>,
    pub input_accepted: usize,
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DuplexPtyEvent {
    Output(Vec<u8>),
    Exit(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DuplexPtyCommand {
    Input(Vec<u8>),
    Resize(PtySize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DuplexPtyOutcome {
    Exited(String),
    Detached,
    Last,
    Next,
    Previous,
    Switcher,
    Terminate,
}

/// An exchange-driven PTY process. The caller supplies bounded input and drains
/// bounded output through `exchange`; dropping it terminates the child.
pub struct RemotePtyProcess {
    master: OwnedFd,
    child: Child,
    exit_status: Option<String>,
    master_closed: bool,
}

impl RemotePtyProcess {
    pub fn spawn(
        command: &str,
        cwd: &Path,
        size: PtySize,
        terminal_type: Option<&str>,
    ) -> Result<Self> {
        if command.trim().is_empty() {
            bail!("PTY command is empty");
        }
        let (master, mut child) = spawn_child(command, cwd, None, size.into(), terminal_type)?;
        if let Err(error) = set_nonblocking(master.as_raw_fd()) {
            terminate(&mut child);
            return Err(error);
        }
        Ok(Self {
            master,
            child,
            exit_status: None,
            master_closed: false,
        })
    }

    pub fn exchange(
        &mut self,
        input: &[u8],
        size: PtySize,
        wait: Duration,
        output_limit: usize,
    ) -> Result<RemotePtyChunk> {
        if output_limit == 0 {
            bail!("PTY output limit must be positive");
        }
        set_pty_size(self.master.as_raw_fd(), size.into())?;
        let input_accepted = write_available(self.master.as_raw_fd(), input)?;
        let mut output = vec![0_u8; output_limit];
        let count = read_available(
            self.master.as_raw_fd(),
            &mut output,
            wait,
            &mut self.master_closed,
        )?;
        output.truncate(count);
        if self.exit_status.is_none()
            && let Some(status) = self
                .child
                .try_wait()
                .context("cannot inspect PTY command")?
        {
            self.exit_status = Some(status.to_string());
        }
        let status = self
            .master_closed
            .then(|| self.exit_status.clone())
            .flatten();
        Ok(RemotePtyChunk {
            output,
            input_accepted,
            status,
        })
    }

    pub fn terminate(&mut self) {
        terminate(&mut self.child);
        self.master_closed = true;
        if self.exit_status.is_none() {
            self.exit_status = Some("terminated".into());
        }
    }
}

impl Drop for RemotePtyProcess {
    fn drop(&mut self) {
        if !self.master_closed || self.exit_status.is_none() {
            self.terminate();
        }
    }
}

/// Relay the controller terminal through a request/response transport. The
/// callback must return the number of leading input bytes accepted by the
/// remote PTY, any output bytes, and the final status when the PTY closes.
pub fn relay_remote(
    mut exchange: impl FnMut(&[u8], PtySize, Duration) -> Result<RemotePtyChunk>,
) -> Result<String> {
    if !controller_is_terminal() {
        bail!("remote PTY execution requires terminal stdin and stdout");
    }
    let terminal = io::stdin();
    let terminal_fd = terminal.as_raw_fd();
    let original = tcgetattr(terminal.as_fd()).context("cannot read terminal attributes")?;
    let mut guard = TerminalGuard::enter(terminal_fd, original)?;
    let result = relay_remote_inner(terminal_fd, io::stdout(), &mut exchange);
    guard.restore()?;
    result
}

/// Relay a framed, full-duplex PTY transport while keeping the controller
/// terminal in raw mode. `transport_fd` must become readable whenever
/// `receive` can consume one complete event.
pub fn relay_duplex(
    transport_fd: RawFd,
    escape_prefix: u8,
    mut send: impl FnMut(DuplexPtyCommand) -> Result<()>,
    mut receive: impl FnMut() -> Result<DuplexPtyEvent>,
) -> Result<DuplexPtyOutcome> {
    if !controller_is_terminal() {
        bail!("duplex PTY execution requires terminal stdin and stdout");
    }
    let terminal = io::stdin();
    let terminal_fd = terminal.as_raw_fd();
    let original = tcgetattr(terminal.as_fd()).context("cannot read terminal attributes")?;
    let mut guard = TerminalGuard::enter(terminal_fd, original)?;
    let result = relay_duplex_inner(
        terminal_fd,
        transport_fd,
        escape_prefix,
        io::stdout(),
        &mut send,
        &mut receive,
    );
    guard.restore()?;
    result
}

fn relay_duplex_inner(
    terminal_fd: RawFd,
    transport_fd: RawFd,
    escape_prefix: u8,
    mut output: impl Write,
    send: &mut impl FnMut(DuplexPtyCommand) -> Result<()>,
    receive: &mut impl FnMut() -> Result<DuplexPtyEvent>,
) -> Result<DuplexPtyOutcome> {
    let mut size = terminal_size(terminal_fd)
        .map(PtySize::from)
        .unwrap_or_default();
    let mut size_sent = false;
    let mut input = vec![0_u8; REMOTE_INPUT_BYTES];
    let mut terminal_open = true;
    let mut prefix_pending = false;
    loop {
        let mut descriptors = [
            libc::pollfd {
                fd: terminal_fd,
                events: if terminal_open { libc::POLLIN } else { 0 },
                revents: 0,
            },
            libc::pollfd {
                fd: transport_fd,
                events: libc::POLLIN | libc::POLLHUP,
                revents: 0,
            },
        ];
        let timeout = if size_sent {
            i32::try_from(POLL_INTERVAL.as_millis()).unwrap_or(100)
        } else {
            0
        };
        let polled = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, timeout) };
        if polled < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error).context("duplex PTY poll failed");
            }
        }

        // Read a queued exit before sending the initial resize. A completed job
        // can close its write side between attachment and entering this relay;
        // treating the resize write as authoritative would surface EPIPE and
        // discard the valid exit frame already waiting in the read buffer.
        if descriptors[1].revents & libc::POLLIN != 0 {
            match receive()? {
                DuplexPtyEvent::Output(bytes) => {
                    output.write_all(&bytes)?;
                    output.flush()?;
                }
                DuplexPtyEvent::Exit(status) => return Ok(DuplexPtyOutcome::Exited(status)),
                DuplexPtyEvent::Error(message) => bail!("remote PTY failed: {message}"),
            }
        } else if descriptors[1].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            bail!("duplex PTY transport closed before reporting status");
        }

        if !size_sent {
            if !send_duplex_command(send, DuplexPtyCommand::Resize(size))? {
                continue;
            }
            size_sent = true;
        }

        if descriptors[0].revents & libc::POLLIN != 0 {
            let count =
                unsafe { libc::read(terminal_fd, input.as_mut_ptr().cast(), input.len() as _) };
            if count > 0 {
                let (bytes, action) = route_escape_input(
                    &input[..count as usize],
                    escape_prefix,
                    &mut prefix_pending,
                );
                if !bytes.is_empty() && !send_duplex_command(send, DuplexPtyCommand::Input(bytes))?
                {
                    continue;
                }
                if let Some(action) = action {
                    if action == EscapeAction::Help {
                        output.write_all(
                            b"\r\n[xshell: d detach | s switch | l last | n/p next/previous | q terminate | ? help]\r\n",
                        )?;
                        output.flush()?;
                        if !send_duplex_command(send, DuplexPtyCommand::Resize(size))? {
                            continue;
                        }
                    } else {
                        return Ok(action.into());
                    }
                }
            } else if count == 0 {
                terminal_open = false;
            } else {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted
                    && error.kind() != io::ErrorKind::WouldBlock
                {
                    return Err(error).context("cannot read controller terminal");
                }
            }
        }

        let updated = terminal_size(terminal_fd)
            .map(PtySize::from)
            .unwrap_or(size);
        if updated != size {
            if !send_duplex_command(send, DuplexPtyCommand::Resize(updated))? {
                continue;
            }
            size = updated;
        }
    }
}

fn send_duplex_command(
    send: &mut impl FnMut(DuplexPtyCommand) -> Result<()>,
    command: DuplexPtyCommand,
) -> Result<bool> {
    match send(command) {
        Ok(()) => Ok(true),
        Err(error) if is_closed_transport(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

fn is_closed_transport(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::NotConnected
            )
        })
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeAction {
    Detach,
    Last,
    Next,
    Previous,
    Switcher,
    Terminate,
    Help,
}

impl From<EscapeAction> for DuplexPtyOutcome {
    fn from(action: EscapeAction) -> Self {
        match action {
            EscapeAction::Detach => Self::Detached,
            EscapeAction::Last => Self::Last,
            EscapeAction::Next => Self::Next,
            EscapeAction::Previous => Self::Previous,
            EscapeAction::Switcher => Self::Switcher,
            EscapeAction::Terminate => Self::Terminate,
            EscapeAction::Help => unreachable!("help does not leave the PTY relay"),
        }
    }
}

fn route_escape_input(
    input: &[u8],
    prefix: u8,
    prefix_pending: &mut bool,
) -> (Vec<u8>, Option<EscapeAction>) {
    let mut forwarded = Vec::with_capacity(input.len() + 1);
    for &byte in input {
        if !*prefix_pending {
            if byte == prefix {
                *prefix_pending = true;
            } else {
                forwarded.push(byte);
            }
            continue;
        }
        *prefix_pending = false;
        if byte == prefix {
            forwarded.push(prefix);
            continue;
        }
        let action = match byte.to_ascii_lowercase() {
            b'd' => Some(EscapeAction::Detach),
            b'l' => Some(EscapeAction::Last),
            b'n' => Some(EscapeAction::Next),
            b'p' => Some(EscapeAction::Previous),
            b's' => Some(EscapeAction::Switcher),
            b'q' => Some(EscapeAction::Terminate),
            b'?' => Some(EscapeAction::Help),
            _ => None,
        };
        if let Some(action) = action {
            return (forwarded, Some(action));
        }
        forwarded.extend_from_slice(&[prefix, byte]);
    }
    (forwarded, None)
}

fn relay_remote_inner(
    terminal_fd: RawFd,
    mut output: impl Write,
    exchange: &mut impl FnMut(&[u8], PtySize, Duration) -> Result<RemotePtyChunk>,
) -> Result<String> {
    let mut pending = Vec::new();
    loop {
        if pending.len() < REMOTE_INPUT_BYTES {
            let mut input = vec![0_u8; REMOTE_INPUT_BYTES - pending.len()];
            let count = read_terminal_available(terminal_fd, &mut input)?;
            pending.extend_from_slice(&input[..count]);
        }
        let size = terminal_size(terminal_fd)
            .map(PtySize::from)
            .unwrap_or_default();
        let chunk = exchange(&pending, size, REMOTE_WAIT)?;
        if chunk.input_accepted > pending.len() {
            bail!("remote PTY accepted an invalid input length");
        }
        pending.drain(..chunk.input_accepted);
        output.write_all(&chunk.output)?;
        output.flush()?;
        if let Some(status) = chunk.status {
            return Ok(status);
        }
    }
}

/// Whether an interactive PTY can be attached to the controller's terminal.
pub fn controller_is_terminal() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

pub fn controller_size() -> Option<PtySize> {
    controller_is_terminal()
        .then(|| terminal_size(io::stdin().as_raw_fd()).map(PtySize::from))
        .flatten()
}

/// Run a user-entered shell command in a transient pseudoterminal and relay the
/// controller terminal byte-for-byte until the command exits.
pub fn run(command: &str, cwd: &Path) -> Result<ExitStatus> {
    if command.trim().is_empty() {
        bail!("PTY command is empty");
    }
    if !controller_is_terminal() {
        bail!("PTY execution requires terminal stdin and stdout");
    }

    let terminal = io::stdin();
    let terminal_fd = terminal.as_raw_fd();
    let original = tcgetattr(terminal.as_fd()).context("cannot read terminal attributes")?;
    let initial_size = terminal_size(terminal_fd).unwrap_or(Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    });
    let (master, mut child) = spawn_child(command, cwd, Some(&original), initial_size, None)?;
    let mut guard = match TerminalGuard::enter(terminal_fd, original) {
        Ok(guard) => guard,
        Err(error) => {
            terminate(&mut child);
            return Err(error);
        }
    };
    let result = relay(&master, child, terminal_fd, io::stdout(), initial_size);
    guard.restore()?;
    result
}

fn spawn_child(
    command: &str,
    cwd: &Path,
    terminal: Option<&Termios>,
    size: Winsize,
    terminal_type: Option<&str>,
) -> Result<(OwnedFd, Child)> {
    let OpenptyResult { master, slave } =
        openpty(Some(&size), terminal).context("cannot allocate pseudoterminal")?;
    let stdin = File::from(slave.try_clone().context("cannot clone PTY slave")?);
    let stdout = File::from(slave.try_clone().context("cannot clone PTY slave")?);
    let stderr = File::from(slave);
    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut process = Command::new(&shell);
    process
        .arg("-lc")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    if let Some(terminal_type) = terminal_type {
        process.env("TERM", terminal_type);
    }
    // Command performs its stdio duplication before this callback. Creating a
    // new session and claiming fd 0 makes the PTY slave the controlling
    // terminal, so its line discipline delivers terminal-generated signals
    // and SIGWINCH to
    // the foreground command rather than xshell.
    unsafe {
        process.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = process
        .spawn()
        .with_context(|| format!("could not launch PTY shell {shell}"))?;
    Ok((master, child))
}

fn relay(
    master: &OwnedFd,
    mut child: Child,
    terminal_fd: RawFd,
    mut output: impl Write,
    size: Winsize,
) -> Result<ExitStatus> {
    if let Err(error) = relay_bytes(master, terminal_fd, &mut output, size) {
        terminate(&mut child);
        return Err(error);
    }
    child.wait().context("cannot wait for PTY command")
}

fn relay_bytes(
    master: &OwnedFd,
    terminal_fd: RawFd,
    mut output: impl Write,
    mut size: Winsize,
) -> Result<()> {
    let master_fd = master.as_raw_fd();
    let mut terminal_open = true;
    let mut master_open = true;
    let mut buffer = vec![0_u8; RELAY_BUFFER_BYTES];

    while master_open {
        if let Some(updated) = terminal_size(terminal_fd)
            && updated != size
        {
            set_pty_size(master_fd, updated)?;
            size = updated;
        }

        let mut descriptors = [
            libc::pollfd {
                fd: terminal_fd,
                events: if terminal_open { libc::POLLIN } else { 0 },
                revents: 0,
            },
            libc::pollfd {
                fd: master_fd,
                events: libc::POLLIN | libc::POLLHUP,
                revents: 0,
            },
        ];
        let timeout = i32::try_from(POLL_INTERVAL.as_millis()).unwrap_or(100);
        let polled =
            unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, timeout) };
        if polled < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error).context("PTY relay poll failed");
            }
        }

        if descriptors[0].revents & libc::POLLIN != 0 {
            let count =
                unsafe { libc::read(terminal_fd, buffer.as_mut_ptr().cast(), buffer.len() as _) };
            if count > 0 {
                write_all_fd(master_fd, &buffer[..count as usize])?;
            } else if count == 0 {
                terminal_open = false;
            } else {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted
                    && error.kind() != io::ErrorKind::WouldBlock
                {
                    return Err(error).context("cannot read controller terminal");
                }
            }
        }

        if descriptors[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let count =
                unsafe { libc::read(master_fd, buffer.as_mut_ptr().cast(), buffer.len() as _) };
            if count > 0 {
                output.write_all(&buffer[..count as usize])?;
                output.flush()?;
            } else if count == 0 {
                master_open = false;
            } else {
                let error = io::Error::last_os_error();
                // Linux PTY masters report EIO after the last slave closes.
                if error.raw_os_error() == Some(libc::EIO) {
                    master_open = false;
                } else if error.kind() != io::ErrorKind::Interrupted
                    && error.kind() != io::ErrorKind::WouldBlock
                {
                    return Err(error).context("cannot read pseudoterminal");
                }
            }
        }
    }
    Ok(())
}

fn terminal_size(descriptor: RawFd) -> Option<Winsize> {
    let mut size = Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe { libc::ioctl(descriptor, libc::TIOCGWINSZ as _, &mut size) };
    (result == 0 && size.ws_row > 0 && size.ws_col > 0).then_some(size)
}

fn set_pty_size(descriptor: RawFd, size: Winsize) -> Result<()> {
    if unsafe { libc::ioctl(descriptor, libc::TIOCSWINSZ as _, &size) } < 0 {
        return Err(io::Error::last_os_error()).context("cannot resize pseudoterminal");
    }
    Ok(())
}

fn write_all_fd(descriptor: RawFd, mut bytes: &[u8]) -> Result<()> {
    while !bytes.is_empty() {
        let count = unsafe { libc::write(descriptor, bytes.as_ptr().cast(), bytes.len()) };
        if count > 0 {
            bytes = &bytes[count as usize..];
            continue;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error).context("cannot write pseudoterminal input");
        }
    }
    Ok(())
}

fn set_nonblocking(descriptor: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error()).context("cannot inspect PTY flags");
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error()).context("cannot set PTY nonblocking mode");
    }
    Ok(())
}

fn write_available(descriptor: RawFd, bytes: &[u8]) -> Result<usize> {
    if bytes.is_empty() {
        return Ok(0);
    }
    let count = unsafe { libc::write(descriptor, bytes.as_ptr().cast(), bytes.len()) };
    if count >= 0 {
        return Ok(count as usize);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::Interrupted || error.kind() == io::ErrorKind::WouldBlock {
        return Ok(0);
    }
    Err(error).context("cannot write remote PTY input")
}

fn read_available(
    descriptor: RawFd,
    output: &mut [u8],
    wait: Duration,
    closed: &mut bool,
) -> Result<usize> {
    if *closed {
        return Ok(0);
    }
    let timeout = i32::try_from(wait.as_millis()).unwrap_or(i32::MAX);
    let mut poll = libc::pollfd {
        fd: descriptor,
        events: libc::POLLIN | libc::POLLHUP,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut poll, 1, timeout) };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(0);
        }
        return Err(error).context("cannot poll remote PTY output");
    }
    if poll.revents & libc::POLLNVAL != 0 {
        bail!("remote PTY descriptor became invalid");
    }
    if result == 0 || poll.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
        return Ok(0);
    }
    let count = unsafe { libc::read(descriptor, output.as_mut_ptr().cast(), output.len()) };
    if count > 0 {
        return Ok(count as usize);
    }
    if count == 0 {
        *closed = true;
        return Ok(0);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EIO) {
        *closed = true;
        return Ok(0);
    }
    if error.kind() == io::ErrorKind::Interrupted || error.kind() == io::ErrorKind::WouldBlock {
        return Ok(0);
    }
    Err(error).context("cannot read remote PTY output")
}

fn read_terminal_available(descriptor: RawFd, output: &mut [u8]) -> Result<usize> {
    let mut poll = libc::pollfd {
        fd: descriptor,
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut poll, 1, 0) };
    if result <= 0 || poll.revents & libc::POLLIN == 0 {
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error).context("cannot poll controller terminal");
            }
        }
        return Ok(0);
    }
    let count = unsafe { libc::read(descriptor, output.as_mut_ptr().cast(), output.len()) };
    if count >= 0 {
        return Ok(count as usize);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::Interrupted || error.kind() == io::ErrorKind::WouldBlock {
        return Ok(0);
    }
    Err(error).context("cannot read controller terminal")
}

fn terminate(child: &mut Child) {
    let killed_group = i32::try_from(child.id()).is_ok_and(|pid| {
        // `spawn_child` calls setsid(), so the shell leader is also the PTY
        // process-group leader. Kill the group so a pager or pipeline cannot
        // survive after its controller disconnects.
        (unsafe { libc::kill(-pid, libc::SIGKILL) }) == 0
    });
    if !killed_group {
        let _ = child.kill();
    }
    let _ = child.wait();
}

struct TerminalGuard {
    descriptor: RawFd,
    original: Option<Termios>,
}

impl TerminalGuard {
    fn enter(descriptor: RawFd, original: Termios) -> Result<Self> {
        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(descriptor) };
        tcsetattr(borrowed, SetArg::TCSANOW, &raw).context("cannot enter terminal raw mode")?;
        Ok(Self {
            descriptor,
            original: Some(original),
        })
    }

    fn restore(&mut self) -> Result<()> {
        let Some(original) = self.original.take() else {
            return Ok(());
        };
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(self.descriptor) };
        tcsetattr(borrowed, SetArg::TCSANOW, &original)
            .context("cannot restore terminal attributes")
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use tempfile::TempDir;

    #[test]
    fn parses_and_routes_configurable_escape_prefix() {
        let prefix = parse_escape_prefix("Ctrl-]").unwrap();
        assert_eq!(prefix, 0x1d);
        let mut pending = false;
        assert_eq!(
            route_escape_input(b"abc", prefix, &mut pending),
            (b"abc".to_vec(), None)
        );
        assert_eq!(
            route_escape_input(&[prefix], prefix, &mut pending),
            (Vec::new(), None)
        );
        assert!(pending);
        assert_eq!(
            route_escape_input(b"d", prefix, &mut pending),
            (Vec::new(), Some(EscapeAction::Detach))
        );
        assert_eq!(
            route_escape_input(&[prefix, prefix], prefix, &mut pending),
            (vec![prefix], None)
        );
        assert!(parse_escape_prefix("not-a-key").is_err());
    }

    #[test]
    fn completed_transport_is_read_before_the_initial_resize() {
        let OpenptyResult { master, slave } = openpty(None, None).unwrap();
        let (transport, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(b"exit-ready").unwrap();
        let mut sent = false;
        let outcome = relay_duplex_inner(
            slave.as_raw_fd(),
            transport.as_raw_fd(),
            0x1d,
            Vec::new(),
            &mut |_| {
                sent = true;
                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "closed").into())
            },
            &mut || Ok(DuplexPtyEvent::Exit("exit status: 0".into())),
        )
        .unwrap();
        drop(master);
        assert_eq!(outcome, DuplexPtyOutcome::Exited("exit status: 0".into()));
        assert!(!sent);
    }

    #[test]
    fn recognizes_wrapped_closed_transport_errors() {
        let error = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "closed",
        ))
        .context("cannot send resize");
        assert!(is_closed_transport(&error));
    }

    #[test]
    fn spawned_command_has_a_tty_and_requested_cwd() {
        let temporary = TempDir::new().unwrap();
        let size = Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let (master, mut child) = spawn_child(
            "test -t 0 && printf 'tty\\n'; read value; printf 'got:%s\\n' \"$value\"; stty size; pwd",
            temporary.path(),
            None,
            size,
            None,
        )
        .unwrap();
        let mut reader = File::from(master);
        reader.write_all(b"hello\n").unwrap();
        let mut output = String::new();
        reader.read_to_string(&mut output).unwrap_or_else(|error| {
            assert_eq!(error.raw_os_error(), Some(libc::EIO));
            0
        });
        assert!(child.wait().unwrap().success());
        assert!(output.contains("tty"));
        assert!(output.contains("got:hello"));
        assert!(output.contains("24 80"));
        assert!(output.contains(temporary.path().to_str().unwrap()));
    }

    #[test]
    fn remote_process_exchanges_bounded_binary_chunks() {
        let temporary = TempDir::new().unwrap();
        let mut process = RemotePtyProcess::spawn(
            "read value; printf 'got:%s\\n' \"$value\"; stty size",
            temporary.path(),
            PtySize {
                rows: 25,
                columns: 90,
            },
            Some("xterm-256color"),
        )
        .unwrap();
        let mut pending = b"remote\n".to_vec();
        let mut output = Vec::new();
        let status = loop {
            let chunk = process
                .exchange(
                    &pending,
                    PtySize {
                        rows: 30,
                        columns: 100,
                    },
                    Duration::from_millis(100),
                    64 * 1024,
                )
                .unwrap();
            pending.drain(..chunk.input_accepted);
            output.extend_from_slice(&chunk.output);
            if let Some(status) = chunk.status {
                break status;
            }
        };
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("got:remote"));
        assert!(output.contains("30 100"));
        assert_eq!(status, "exit status: 0");
    }

    #[test]
    fn controller_relay_forwards_pending_input_and_output() {
        let size = Winsize {
            ws_row: 27,
            ws_col: 91,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let OpenptyResult { master, slave } = openpty(Some(&size), None).unwrap();
        let mut controller = File::from(master);
        controller.write_all(b"answer\n").unwrap();
        let mut output = Vec::new();
        let mut called = false;
        let status = relay_remote_inner(
            slave.as_raw_fd(),
            &mut output,
            &mut |input, observed_size, wait| {
                assert_eq!(input, b"answer\n");
                assert_eq!(
                    observed_size,
                    PtySize {
                        rows: 27,
                        columns: 91
                    }
                );
                assert_eq!(wait, REMOTE_WAIT);
                called = true;
                Ok(RemotePtyChunk {
                    output: b"rendered".to_vec(),
                    input_accepted: input.len(),
                    status: Some("exit status: 0".into()),
                })
            },
        )
        .unwrap();
        assert!(called);
        assert_eq!(output, b"rendered");
        assert_eq!(status, "exit status: 0");
    }
}

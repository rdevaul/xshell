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

/// Whether an interactive PTY can be attached to the controller's terminal.
pub fn controller_is_terminal() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
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
    let (master, mut child) = spawn(command, cwd, Some(&original), initial_size)?;
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

fn spawn(
    command: &str,
    cwd: &Path,
    terminal: Option<&Termios>,
    size: Winsize,
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
    // Command performs its stdio duplication before this callback. Creating a
    // new session and claiming fd 0 makes the PTY slave the controlling
    // terminal, so its line discipline delivers Ctrl-C/Ctrl-Z and SIGWINCH to
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

fn terminate(child: &mut Child) {
    let _ = child.kill();
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
    use tempfile::TempDir;

    #[test]
    fn spawned_command_has_a_tty_and_requested_cwd() {
        let temporary = TempDir::new().unwrap();
        let size = Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let (master, mut child) = spawn(
            "test -t 0 && printf 'tty\\n'; read value; printf 'got:%s\\n' \"$value\"; stty size; pwd",
            temporary.path(),
            None,
            size,
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
}

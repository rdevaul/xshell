//! Small, dependency-light Unix helpers shared by the xshell daemons.
//!
//! Both `xshelld` and `xshell-auditd` accept connections on per-user Unix
//! sockets and keep state in per-user directories. The checks here make the
//! trust boundary explicit instead of relying on `chmod` failing by accident.

use anyhow::{Context, Result, bail};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;

pub mod descriptors;
pub mod service;

/// The effective user ID of this process.
pub fn effective_uid() -> u32 {
    // SAFETY: geteuid(2) has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Return the UID of the process on the other end of a connected Unix socket.
#[cfg(target_os = "linux")]
pub fn peer_uid(stream: &UnixStream) -> Result<u32> {
    use std::mem::{size_of, zeroed};
    use std::os::fd::AsRawFd;

    // SAFETY: `ucred` is plain-old-data, and getsockopt writes at most
    // `length` bytes into it.
    let mut credentials: libc::ucred = unsafe { zeroed() };
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `credentials` is a writable `ucred` and `length` holds its size, so getsockopt
    // writes within it.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot identify socket peer");
    }
    Ok(credentials.uid)
}

/// Return the UID of the process on the other end of a connected Unix socket.
#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
))]
pub fn peer_uid(stream: &UnixStream) -> Result<u32> {
    use std::os::fd::AsRawFd;

    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: getpeereid(2) writes to the two out-pointers we own.
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot identify socket peer");
    }
    Ok(uid)
}

/// Return the process ID of the process on the other end of a connected
/// Unix socket, as recorded when it connected.
#[cfg(target_os = "linux")]
pub fn peer_pid(stream: &UnixStream) -> Result<u32> {
    use std::mem::{size_of, zeroed};
    use std::os::fd::AsRawFd;

    // SAFETY: `ucred` is plain-old-data, and getsockopt writes at most
    // `length` bytes into it.
    let mut credentials: libc::ucred = unsafe { zeroed() };
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `credentials` is a writable `ucred` and `length` holds its
    // size, so getsockopt writes within it.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot identify socket peer");
    }
    u32::try_from(credentials.pid).context("socket peer has no process ID")
}

/// Return the process ID of the process on the other end of a connected
/// Unix socket, as recorded when it connected.
#[cfg(target_os = "macos")]
pub fn peer_pid(stream: &UnixStream) -> Result<u32> {
    use std::mem::size_of;
    use std::os::fd::AsRawFd;

    let mut pid: libc::pid_t = 0;
    let mut length = size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: `pid` is a writable pid_t and `length` holds its size, so
    // getsockopt writes within it.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot identify socket peer");
    }
    u32::try_from(pid).context("socket peer has no process ID")
}

/// The parent of process `pid`, or `None` when it cannot be read (the
/// process has exited, or the platform does not say).
#[cfg(target_os = "linux")]
pub fn parent_pid(pid: u32) -> Option<u32> {
    // The command name in parentheses may contain spaces or `)`, so parse
    // from the last `)`: "pid (comm) state ppid ...".
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// The parent of process `pid`, or `None` when it cannot be read (the
/// process has exited, or the platform does not say).
#[cfg(target_os = "macos")]
pub fn parent_pid(pid: u32) -> Option<u32> {
    use std::mem::{size_of, zeroed};

    // SAFETY: `proc_bsdinfo` is plain-old-data that proc_pidinfo fills.
    let mut info: libc::proc_bsdinfo = unsafe { zeroed() };
    let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is a writable buffer of `size` bytes.
    let written = unsafe {
        libc::proc_pidinfo(
            libc::c_int::try_from(pid).ok()?,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (written == size).then_some(info.pbi_ppid)
}

/// Whether process `pid` may be `ancestor` itself or one of its
/// descendants, walking parent links at most a bounded number of steps.
///
/// This fails closed: it answers `false` only when the walk reaches the
/// root (pid 1), or a process owned by another user, without meeting
/// `ancestor`. Any other process whose ancestry cannot be read counts as a
/// descendant. Otherwise a descendant could connect,
/// hand the connection to a child and exit before the check, so the walk
/// would find no process and wave the connection through.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn is_descendant_of(pid: u32, ancestor: u32) -> bool {
    const MAX_DEPTH: usize = 256;
    let mut current = pid;
    for _ in 0..MAX_DEPTH {
        if current == ancestor {
            return true;
        }
        if current <= 1 {
            return false;
        }
        match parent_pid(current) {
            Some(parent) if parent != current => current = parent,
            Some(_) => return true,
            // Unreadable. A process that exists but belongs to another user
            // (an ancestor such as `login`) is outside the daemon's tree:
            // everything the daemon starts runs as the daemon's user. Any
            // other failure (the process has exited, say) is not known to be
            // outside it.
            None => return !belongs_to_another_user(current),
        }
    }
    true
}

/// Whether process `pid` exists but this process may not signal it, which
/// means another user owns it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn belongs_to_another_user(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: kill(2) with signal 0 sends nothing; it only reports whether
    // the process exists and may be signalled.
    let result = unsafe { libc::kill(pid, 0) };
    result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Make this process the reaper of its orphaned descendants (Linux), so a
/// child that double-forks to leave the process tree is re-parented here
/// and stays a descendant. Elsewhere this does nothing and returns `false`.
pub fn become_child_subreaper() -> bool {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: prctl(PR_SET_CHILD_SUBREAPER) takes integer arguments and
        // has no memory-safety preconditions.
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) == 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Reject a connection whose peer is not the same user as this process.
///
/// Socket file mode alone is not a complete control: a socket can be reached
/// through a directory the caller does not own, and some platforms ignore the
/// socket inode's mode entirely. Daemons that grant shell access as the
/// invoking user must check the peer explicitly.
pub fn require_same_user(stream: &UnixStream, what: &str) -> Result<u32> {
    let uid = peer_uid(stream)?;
    let own = effective_uid();
    if uid != own {
        bail!("{what} connection from uid {uid} rejected; this service only accepts uid {own}");
    }
    Ok(uid)
}

/// Ensure `path` is a real directory owned by the current user and not
/// writable by group or others. Creates it with mode 0700 if it does not
/// exist. Refuses symlinks so a pre-planted link cannot redirect state into
/// another location.
pub fn ensure_secure_directory(path: &Path, what: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("{what} path {} must be a real directory", path.display());
            }
            if metadata.uid() != effective_uid() {
                bail!(
                    "{what} directory {} is not owned by the current user",
                    path.display()
                );
            }
            if metadata.mode() & 0o022 != 0 {
                bail!(
                    "{what} directory {} is group/world writable",
                    path.display()
                );
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "cannot create parent of {what} directory {}",
                        path.display()
                    )
                })?;
            }
            // create_dir (not create_dir_all) so that a directory racing into
            // existence between the metadata check and here is an error rather
            // than silently adopted with unknown ownership.
            fs::create_dir(path)
                .with_context(|| format!("cannot create {what} directory {}", path.display()))?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("cannot secure {what} directory {}", path.display()))
        }
        Err(error) => Err(error)
            .with_context(|| format!("cannot inspect {what} directory {}", path.display())),
    }
}

/// Lock a `std::sync::Mutex`, recovering the guard if a previous holder
/// panicked.
///
/// The daemons here serve many independent client threads. With the default
/// `lock().expect(..)` pattern, one panic while holding a lock poisons it and
/// every later thread that touches the same state panics too: the process
/// stays alive, accepts connections, and fails each one. The state guarded by
/// these mutexes is bookkeeping (maps, ring buffers, flags) that remains
/// self-consistent after any single statement, so continuing with the
/// recovered guard is safe and keeps the daemon serviceable.
pub trait LockExt<T> {
    fn lock_recover(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> LockExt<T> for std::sync::Mutex<T> {
    fn lock_recover(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                eprintln!(
                    "warning: recovered a poisoned lock after a panic in another thread; \
continuing with the last consistent state"
                );
            });
            poisoned.into_inner()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use tempfile::TempDir;

    #[test]
    fn creates_missing_directory_with_private_mode() {
        let temporary = TempDir::new().unwrap();
        let target = temporary.path().join("nested/state");
        ensure_secure_directory(&target, "test").unwrap();
        let mode = fs::metadata(&target).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700);
        // Idempotent on an existing secure directory.
        ensure_secure_directory(&target, "test").unwrap();
    }

    #[test]
    fn rejects_world_writable_and_symlinked_directories() {
        let temporary = TempDir::new().unwrap();
        let loose = temporary.path().join("loose");
        fs::create_dir(&loose).unwrap();
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(ensure_secure_directory(&loose, "test").is_err());

        let real = temporary.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = temporary.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(ensure_secure_directory(&link, "test").is_err());

        let file = temporary.path().join("file");
        fs::write(&file, b"x").unwrap();
        assert!(ensure_secure_directory(&file, "test").is_err());
    }

    #[test]
    fn lock_recover_continues_after_a_poisoning_panic() {
        use std::sync::{Arc, Mutex};
        let shared = Arc::new(Mutex::new(vec![1, 2, 3]));
        let poisoner = Arc::clone(&shared);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert!(shared.lock().is_err(), "mutex should be poisoned");
        assert_eq!(shared.lock_recover().len(), 3);
    }

    #[test]
    fn peer_pid_names_this_process_over_a_socket_pair() {
        let (client, _server) = UnixStream::pair().unwrap();
        assert_eq!(peer_pid(&client).unwrap(), std::process::id());
    }

    #[test]
    fn a_child_is_a_descendant_and_the_parent_is_not() {
        let me = std::process::id();
        assert!(is_descendant_of(me, me));
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        assert!(is_descendant_of(child.id(), me));
        assert!(!is_descendant_of(me, child.id()));
        if let Some(parent) = parent_pid(me) {
            assert!(!is_descendant_of(parent, me));
        }
        let gone = child.id();
        let _ = child.kill();
        let _ = child.wait();
        // A process that has exited cannot be placed outside the tree, so it
        // counts as a descendant: a descendant must not escape the check by
        // handing its connection on and exiting first (F27).
        assert!(is_descendant_of(gone, u32::MAX - 1));
    }

    #[test]
    fn peer_uid_matches_own_uid_over_loopback_socket() {
        let temporary = TempDir::new().unwrap();
        let socket = temporary.path().join("s.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let client = UnixStream::connect(&socket).unwrap();
        let (server, _) = listener.accept().unwrap();
        assert_eq!(peer_uid(&server).unwrap(), effective_uid());
        assert_eq!(require_same_user(&server, "test").unwrap(), effective_uid());
        drop(client);
    }
}

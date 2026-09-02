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

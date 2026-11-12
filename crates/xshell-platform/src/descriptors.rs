//! Descriptor hygiene for the processes xshell starts (finding F12).
//!
//! `xshelld` holds descriptors that no child should see: every live job's
//! PTY master, client sockets, audit connections. A descriptor without
//! close-on-exec survives `exec`, so a command run in one session could
//! type into, or read, another session's terminal. Two layers keep them
//! out of children:
//!
//! 1. Descriptors are created close-on-exec. The standard library does this
//!    for the files, sockets and pipes it opens; `xshell-pty` does it for
//!    PTY masters and slaves, which `openpty(3)` left inheritable.
//! 2. [`child_hook`] runs in each shell child between `fork` and `exec` and
//!    marks every descriptor above 2 close-on-exec, so a descriptor that
//!    missed layer 1, or was opened by another thread during a
//!    non-atomic window, still cannot cross `exec`.
//!
//! The hook marks descriptors close-on-exec rather than closing them. The
//! effect at `exec` is the same, but the standard library's own
//! close-on-exec error pipe, which reports a failed `exec` to the parent,
//! stays open until the `exec` itself. Descriptors 0 to 2 are left alone:
//! `Command` has already duplicated the child's stdio onto them, and
//! `dup2` clears close-on-exec on the copy.

use std::io;
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt;

/// Descriptors at or above this number are swept in each child.
const FIRST_SWEPT_DESCRIPTOR: RawFd = 3;

/// The highest descriptor bound a fallback sweep will walk. A soft
/// `RLIMIT_NOFILE` can be very large (macOS reports 2^20 or "infinity" to
/// many processes), and each probe is one system call.
const FALLBACK_CEILING: RawFd = 1 << 20;

/// Set close-on-exec on one descriptor, keeping its other descriptor flags.
pub fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl(F_GETFD) and fcntl(F_SETFD) read and write only the
    // descriptor flags; a stale descriptor fails with EBADF.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if flags & libc::FD_CLOEXEC == 0
            && libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// State for marking a child's inherited descriptors close-on-exec.
///
/// [`prepare`](Self::prepare) runs in the parent and does everything that
/// may allocate. [`run`](Self::run) runs in the forked child and makes only
/// async-signal-safe system calls on memory prepared in advance.
pub struct DescriptorSweep {
    fallback_limit: RawFd,
    #[cfg(target_os = "macos")]
    listing: Vec<libc::proc_fdinfo>,
}

impl DescriptorSweep {
    /// Size the sweep for this process's current descriptor table.
    pub fn prepare() -> Self {
        Self {
            fallback_limit: fallback_limit(),
            #[cfg(target_os = "macos")]
            listing: macos::listing_buffer(),
        }
    }

    /// Mark every descriptor above 2 close-on-exec.
    ///
    /// This is meant for a `pre_exec` hook. It does not allocate, take a
    /// lock or touch shared state; it calls only `close_range(2)` (Linux),
    /// `proc_pidinfo` (macOS, a wrapper around the `proc_info` system call
    /// writing into a buffer allocated by [`prepare`](Self::prepare)),
    /// `getpid(2)` and `fcntl(2)`. It never fails: a descriptor it cannot
    /// mark is one that is not open.
    pub fn run(&mut self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            // SAFETY: close_range(2) with CLOSE_RANGE_CLOEXEC changes only
            // descriptor flags. Kernels before 5.11 reject the flag (EINVAL)
            // or the call (ENOSYS); the loop below covers them.
            let marked = unsafe {
                libc::syscall(
                    libc::SYS_close_range,
                    FIRST_SWEPT_DESCRIPTOR as libc::c_uint,
                    libc::c_uint::MAX,
                    libc::CLOSE_RANGE_CLOEXEC,
                )
            };
            if marked == 0 {
                return Ok(());
            }
        }
        #[cfg(target_os = "macos")]
        if macos::mark_listed(&mut self.listing) {
            return Ok(());
        }
        probe_every_descriptor(self.fallback_limit);
        Ok(())
    }
}

/// Mark each descriptor from 3 below `limit` close-on-exec, one `fcntl(2)`
/// probe per number. Async-signal-safe.
fn probe_every_descriptor(limit: RawFd) {
    for fd in FIRST_SWEPT_DESCRIPTOR..limit {
        let _ = set_cloexec(fd);
    }
}

/// A `pre_exec` hook that keeps every descriptor above 2 from crossing
/// `exec`. Build it in the parent, immediately before spawning.
///
/// The returned closure is async-signal-safe (see [`DescriptorSweep::run`]).
/// Register it after any hook that needs an inherited descriptor, such as
/// one that `dup2`s a pipe into place.
pub fn child_hook() -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    let mut sweep = DescriptorSweep::prepare();
    move || sweep.run()
}

/// Add [`child_hook`] to a standard-library command.
pub fn restrict_inherited_descriptors(command: &mut std::process::Command) {
    // SAFETY: the hook makes only async-signal-safe system calls on memory
    // allocated before the fork; see `DescriptorSweep::run`.
    unsafe {
        command.pre_exec(child_hook());
    }
}

fn fallback_limit() -> RawFd {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes one `rlimit` into the struct passed.
    let current = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
        limit.rlim_cur
    } else {
        libc::RLIM_INFINITY
    };
    if current == libc::RLIM_INFINITY {
        return FALLBACK_CEILING;
    }
    RawFd::try_from(current)
        .unwrap_or(FALLBACK_CEILING)
        .clamp(FIRST_SWEPT_DESCRIPTOR, FALLBACK_CEILING)
}

#[cfg(target_os = "macos")]
mod macos {
    use std::mem::size_of;

    /// Spare entries beyond the parent's current descriptor count, so a
    /// descriptor opened by another thread before the fork still fits.
    const SPARE_ENTRIES: usize = 256;

    pub(super) fn listing_buffer() -> Vec<libc::proc_fdinfo> {
        // SAFETY: a null buffer asks proc_pidinfo for the size it needs and
        // writes nothing.
        let needed = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDLISTFDS,
                0,
                std::ptr::null_mut(),
                0,
            )
        };
        let entries = usize::try_from(needed).unwrap_or(0) / size_of::<libc::proc_fdinfo>();
        vec![
            libc::proc_fdinfo {
                proc_fd: -1,
                proc_fdtype: 0,
            };
            entries + SPARE_ENTRIES
        ]
    }

    /// Mark each listed descriptor above 2 close-on-exec. Returns false
    /// when the listing failed or may have been truncated, so the caller
    /// falls back to probing every descriptor number.
    pub(super) fn mark_listed(listing: &mut [libc::proc_fdinfo]) -> bool {
        let Ok(capacity) = libc::c_int::try_from(size_of_val(listing)) else {
            return false;
        };
        // SAFETY: `listing` is writable for `capacity` bytes, and
        // proc_pidinfo writes at most that many.
        let written = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDLISTFDS,
                0,
                listing.as_mut_ptr().cast(),
                capacity,
            )
        };
        if written <= 0 || written >= capacity {
            return false;
        }
        let count = written as usize / size_of::<libc::proc_fdinfo>();
        for entry in &listing[..count] {
            if entry.proc_fd >= super::FIRST_SWEPT_DESCRIPTOR {
                let _ = super::set_cloexec(entry.proc_fd);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::process::Command;

    fn leaky_pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        // SAFETY: `fds` is a writable array of two descriptors, as pipe(2)
        // needs; pipe(2) does not set close-on-exec.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: pipe(2) returned two new descriptors nothing else owns.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    fn child_sees(fd: RawFd, restrict: bool) -> bool {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(format!("test -e /dev/fd/{fd}"));
        if restrict {
            restrict_inherited_descriptors(&mut command);
        }
        command.status().unwrap().success()
    }

    #[test]
    fn f12_children_do_not_inherit_descriptors_above_stdio() {
        let (read, write) = leaky_pipe();
        assert!(
            child_sees(read.as_raw_fd(), false),
            "control: an unmarked descriptor should cross exec"
        );
        assert!(!child_sees(read.as_raw_fd(), true));
        assert!(!child_sees(write.as_raw_fd(), true));
    }

    #[test]
    fn f12_the_fallback_sweep_marks_descriptors() {
        let (read, _write) = leaky_pipe();
        let fd = read.as_raw_fd();
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(format!("test -e /dev/fd/{fd}"));
        // Exercise only the probing loop that old kernels fall back to.
        // SAFETY: the hook calls only fcntl(2).
        unsafe {
            command.pre_exec(move || {
                probe_every_descriptor(fd + 1);
                Ok(())
            });
        }
        assert!(!command.status().unwrap().success());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn f12_the_macos_listing_fits_its_prepared_buffer() {
        // Run the fast path alone in a child, where marking descriptors
        // cannot disturb other tests, and fail the spawn if it fell back.
        let (read, _write) = leaky_pipe();
        let fd = read.as_raw_fd();
        let mut listing = macos::listing_buffer();
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(format!("test -e /dev/fd/{fd}"));
        // SAFETY: the hook calls only getpid, proc_pidinfo and fcntl on a
        // buffer allocated before the fork.
        unsafe {
            command.pre_exec(move || {
                if macos::mark_listed(&mut listing) {
                    Ok(())
                } else {
                    Err(std::io::Error::other("descriptor listing fell back"))
                }
            });
        }
        assert!(!command.status().unwrap().success());
    }
}

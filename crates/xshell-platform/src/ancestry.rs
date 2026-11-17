//! Walking a process's parent links, so `xshelld` can refuse connections
//! from the processes it started (F27, F30).
//!
//! The walk must read every real ancestor of the user's own client. On macOS
//! that includes `login`, which is setuid root, and `launchd`. On Linux it
//! includes `sshd` and `su`. An ancestor the walk cannot read would otherwise
//! leave it guessing, and F30 was a wrong guess: the user's client under
//! Terminal.app was refused as one of the daemon's own descendants.

/// What reading one process's parent found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parent {
    /// The process exists and this is its parent.
    Pid(u32),
    /// The process no longer exists.
    Gone,
    /// The process exists, but the system will not say who its parent is.
    Hidden,
}

/// Read the parent of process `pid`.
///
/// `/proc/<pid>/stat` is readable for every process, whoever owns it,
/// unless `/proc` is mounted with `hidepid`. Then another user's process
/// (and a setuid one) cannot be read, and with `hidepid=invisible` it looks
/// absent. `kill(pid, 0)` tells that case apart from a process that has
/// really gone: it reports only whether the pid exists.
#[cfg(target_os = "linux")]
pub fn read_parent(pid: u32) -> Parent {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => parse_stat_parent(&stat).map_or(Parent::Hidden, Parent::Pid),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if process_exists(pid) {
                Parent::Hidden
            } else {
                Parent::Gone
            }
        }
        Err(_) => Parent::Hidden,
    }
}

/// The parent field of a `/proc/<pid>/stat` line.
///
/// The command name in parentheses may contain spaces or `)`, so this parses
/// from the last `)`: "pid (comm) state ppid ...".
#[cfg(target_os = "linux")]
pub(crate) fn parse_stat_parent(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(target_os = "linux")]
fn process_exists(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: kill(2) with signal 0 sends nothing. It reports only whether
    // the process exists (EPERM still means it does).
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Read the parent of process `pid`.
///
/// This uses `proc_pidinfo(PROC_PIDT_SHORTBSDINFO)`, which, like `ps`,
/// reads any process. The fuller `PROC_PIDTBSDINFO` fails with EPERM for a
/// process this user did not start as itself, such as the setuid-root
/// `login` in every Terminal.app shell's ancestry, or `launchd` (F30).
#[cfg(target_os = "macos")]
pub fn read_parent(pid: u32) -> Parent {
    use std::mem::{size_of, zeroed};

    let Ok(raw_pid) = libc::c_int::try_from(pid) else {
        return Parent::Gone;
    };
    // SAFETY: `proc_bsdshortinfo` is plain-old-data that proc_pidinfo fills.
    let mut info: libc::proc_bsdshortinfo = unsafe { zeroed() };
    let size = size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
    // SAFETY: `info` is a writable buffer of `size` bytes.
    let written = unsafe {
        libc::proc_pidinfo(
            raw_pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdshortinfo).cast(),
            size,
        )
    };
    if written == size {
        return Parent::Pid(info.pbsi_ppid);
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Parent::Gone,
        _ => Parent::Hidden,
    }
}

/// The parent of process `pid`, or `None` when it cannot be read.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn parent_pid(pid: u32) -> Option<u32> {
    match read_parent(pid) {
        Parent::Pid(parent) => Some(parent),
        Parent::Gone | Parent::Hidden => None,
    }
}

/// Whether process `pid` is `ancestor` itself or one of its descendants.
///
/// See [`walk`] for how unreadable processes count.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn is_descendant_of(pid: u32, ancestor: u32) -> bool {
    walk(pid, ancestor, read_parent)
}

/// Walk parent links from `pid`, at most a bounded number of steps, and say
/// whether the walk meets `ancestor`.
///
/// - Reaching pid 1 (or 0) without meeting `ancestor` means "not a
///   descendant".
/// - A process that has **gone** counts as a descendant. Otherwise a
///   descendant could connect, hand the connection to a child and exit
///   before the check, and the walk would wave it through.
/// - A process that exists but is **hidden** counts as outside the tree.
///   Everything the daemon starts runs as the daemon's user and can be
///   read: on macOS any process can, and on Linux only `hidepid` hides one,
///   and then only another user's or a setuid one. Counting a hidden
///   ancestor as a descendant would lock the user's own client out under
///   `hidepid`, behind a root `sshd`.
/// - A loop, or a chain longer than the bound, counts as a descendant.
pub fn walk(pid: u32, ancestor: u32, mut parent_of: impl FnMut(u32) -> Parent) -> bool {
    const MAX_DEPTH: usize = 1024;
    let mut current = pid;
    for _ in 0..MAX_DEPTH {
        if current == ancestor {
            return true;
        }
        if current <= 1 {
            return false;
        }
        match parent_of(current) {
            Parent::Pid(parent) if parent != current => current = parent,
            Parent::Pid(_) | Parent::Gone => return true,
            Parent::Hidden => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn tree(links: &[(u32, Parent)]) -> impl FnMut(u32) -> Parent {
        let links: HashMap<u32, Parent> = links.iter().copied().collect();
        move |pid| links.get(&pid).copied().unwrap_or(Parent::Gone)
    }

    #[test]
    fn a_chain_that_reaches_the_ancestor_is_a_descendant() {
        // child 40 → shell 30 → daemon 20 → launchd 1
        let links = [
            (40, Parent::Pid(30)),
            (30, Parent::Pid(20)),
            (20, Parent::Pid(1)),
        ];
        assert!(walk(40, 20, tree(&links)));
        assert!(walk(20, 20, tree(&links)));
        assert!(!walk(20, 40, tree(&links)));
    }

    #[test]
    fn a_chain_through_login_to_launchd_is_not_a_descendant() {
        // The F30 chain: zsh → -zsh → login (setuid root) → Terminal →
        // launchd. Every link is read, so the walk ends at pid 1.
        let links = [
            (500, Parent::Pid(400)),
            (400, Parent::Pid(300)),
            (300, Parent::Pid(200)),
            (200, Parent::Pid(1)),
            (1, Parent::Pid(0)),
        ];
        assert!(!walk(500, 99, tree(&links)));
    }

    #[test]
    fn a_hidden_ancestor_is_outside_the_tree() {
        // `hidepid` on Linux hides a root `sshd` above the user's shell.
        let links = [(500, Parent::Pid(400)), (400, Parent::Hidden)];
        assert!(!walk(500, 99, tree(&links)));
    }

    #[test]
    fn a_vanished_process_counts_as_a_descendant() {
        // The peer exited before the check, perhaps after handing its
        // connection to a child.
        assert!(walk(77, 99, tree(&[])));
        // An intermediate process that exited mid-walk.
        let links = [(500, Parent::Pid(400))];
        assert!(walk(500, 99, tree(&links)));
    }

    #[test]
    fn a_loop_counts_as_a_descendant() {
        assert!(walk(5, 99, tree(&[(5, Parent::Pid(5))])));
        assert!(walk(
            5,
            99,
            tree(&[(5, Parent::Pid(6)), (6, Parent::Pid(5))])
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stat_parsing_skips_awkward_command_names() {
        assert_eq!(parse_stat_parent("42 (a) b) c) S 7 42 42"), Some(7));
        assert_eq!(parse_stat_parent("garbage"), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn this_process_and_its_children_are_placed_correctly() {
        let me = std::process::id();
        assert!(is_descendant_of(me, me));
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        assert!(is_descendant_of(child.id(), me));
        assert!(!is_descendant_of(me, child.id()));
        // Every real ancestor of this test is readable, whoever owns it
        // (under Terminal.app that includes the setuid-root `login`), so this
        // process is not a descendant of its own child.
        let mut current = me;
        while current > 1 {
            let Parent::Pid(parent) = read_parent(current) else {
                panic!("ancestor {current} of the test process cannot be read");
            };
            current = parent;
        }
        let gone = child.id();
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(read_parent(gone), Parent::Gone);
        assert!(is_descendant_of(gone, u32::MAX - 1));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn launchd_or_init_is_readable_and_outside_this_process() {
        assert!(matches!(read_parent(1), Parent::Pid(_)));
        assert!(!is_descendant_of(1, std::process::id()));
    }
}

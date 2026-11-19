//! Keeping `xshelld`'s descendants in its process tree on Linux, and reaping
//! the orphans that brings (F27, F30).
//!
//! As a child subreaper, the daemon adopts every orphaned descendant: a
//! background member of a `run_shell` group, `nohup x &` in a `$` command,
//! anything that daemonizes. Its owner has exited, so nobody else will wait
//! for it, and without a reaper each one stays a zombie of the daemon.
//!
//! The reaper must never collect a child the daemon waits on itself (tokio
//! and std wait on their own pids, and a stolen status turns into ECHILD).
//! So every child the daemon spawns goes through [`spawn_waited`], which
//! records its pid under a gate the reaper also takes. The reaper reaps
//! only zombie children it has no record of. A record is dropped once the
//! pid is no longer the daemon's child, which means its owner reaped it.
//!
//! **Every spawn in a process that starts the reaper must use
//! [`spawn_waited`]**, or its status could be taken before the owner waits.
//! Outside such a process (tests, the CLI) `spawn_waited` only spawns.
//!
//! On other platforms there is no subreaper: everything here does nothing.

#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "linux")]
use std::sync::{Mutex, RwLock};

/// How often the reaper looks for adopted zombies.
#[cfg(target_os = "linux")]
const REAP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Whether the reaper runs in this process, so spawns must be recorded.
#[cfg(target_os = "linux")]
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Spawns hold it shared across spawning and recording a child. The reaper
/// holds it exclusively, so it never sees a child before its record.
#[cfg(target_os = "linux")]
static GATE: RwLock<()> = RwLock::new(());
/// Pids of children the daemon spawned and waits on itself.
#[cfg(target_os = "linux")]
static OWNED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Make this process the reaper of its orphaned descendants (Linux), so a
/// child that double-forks to leave the process tree is re-parented here
/// and stays a descendant. Call [`start_orphan_reaper`] with it. Elsewhere
/// this does nothing and returns `false`.
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

/// Start a thread that reaps adopted orphans (Linux). Call it before this
/// process spawns any child, so every child is recorded. Elsewhere this does
/// nothing and returns `false`.
pub fn start_orphan_reaper() -> bool {
    #[cfg(target_os = "linux")]
    {
        if ACTIVE.swap(true, Ordering::SeqCst) {
            return true;
        }
        std::thread::Builder::new()
            .name("orphan-reaper".into())
            .spawn(|| {
                loop {
                    reap_orphans();
                    std::thread::sleep(REAP_INTERVAL);
                }
            })
            .is_ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Spawn a child that the caller waits on itself, recording its pid so the
/// orphan reaper leaves it alone. `pid_of` names the spawned child's pid.
pub fn spawn_waited<T, E>(
    spawn: impl FnOnce() -> Result<T, E>,
    pid_of: impl FnOnce(&T) -> Option<u32>,
) -> Result<T, E> {
    #[cfg(target_os = "linux")]
    {
        if !ACTIVE.load(Ordering::SeqCst) {
            return spawn();
        }
        let _gate = GATE.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        let child = spawn()?;
        if let Some(pid) = pid_of(&child) {
            OWNED
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(pid);
        }
        Ok(child)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid_of;
        spawn()
    }
}

/// Reap every zombie child of this process that it did not spawn through
/// [`spawn_waited`]. Returns how many were reaped.
#[cfg(target_os = "linux")]
pub fn reap_orphans() -> usize {
    reap_where(|_| true)
}

/// [`reap_orphans`], limited to the children `candidate` accepts, so a test
/// leaves the children of other tests alone.
#[cfg(target_os = "linux")]
fn reap_where(candidate: impl Fn(u32) -> bool) -> usize {
    let me = std::process::id();
    let _gate = GATE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut owned = OWNED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // A record whose pid is no longer this process's child was reaped by
    // its owner. Each record is checked directly: the children lists below
    // can miss an entry, and dropping a live record would expose its child.
    owned.retain(|&pid| !matches!(child_state(pid, me), ChildState::NotAChild));
    let mut reaped = 0;
    for pid in children(me) {
        if !candidate(pid) || owned.contains(&pid) || child_state(pid, me) != ChildState::Zombie {
            continue;
        }
        let Ok(raw) = libc::pid_t::try_from(pid) else {
            continue;
        };
        let mut status = 0;
        // SAFETY: waitpid(2) on one specific pid with WNOHANG writes only
        // the status integer.
        if unsafe { libc::waitpid(raw, &mut status, libc::WNOHANG) } == raw {
            reaped += 1;
        }
    }
    reaped
}

#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
enum ChildState {
    Zombie,
    Running,
    NotAChild,
}

/// Whether `pid` is a child of `parent`, and a zombie.
#[cfg(target_os = "linux")]
fn child_state(pid: u32, parent: u32) -> ChildState {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return ChildState::NotAChild;
    };
    let Some(close) = stat.rfind(')') else {
        return ChildState::Running;
    };
    let mut fields = stat[close + 1..].split_whitespace();
    let state = fields.next();
    match fields.next().and_then(|ppid| ppid.parse::<u32>().ok()) {
        Some(ppid) if ppid != parent => ChildState::NotAChild,
        _ if state == Some("Z") => ChildState::Zombie,
        _ => ChildState::Running,
    }
}

/// The pids of this process's children. It reads each thread's `children`
/// list; a kernel without them gets a scan of every process instead.
#[cfg(target_os = "linux")]
fn children(me: u32) -> Vec<u32> {
    let mut pids = Vec::new();
    let mut listed = false;
    if let Ok(tasks) = std::fs::read_dir("/proc/self/task") {
        for task in tasks.flatten() {
            if let Ok(list) = std::fs::read_to_string(task.path().join("children")) {
                listed = true;
                pids.extend(
                    list.split_whitespace()
                        .filter_map(|pid| pid.parse::<u32>().ok()),
                );
            }
        }
    }
    if !listed && let Ok(entries) = std::fs::read_dir("/proc") {
        pids.extend(
            entries
                .flatten()
                .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
                .filter(|&pid| child_state(pid, me) != ChildState::NotAChild),
        );
    }
    pids.sort_unstable();
    pids.dedup();
    pids
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn only_unrecorded_zombies_are_reaped() {
        let me = std::process::id();
        // Recorded as if the reaper were running, so this child is owned.
        let _gate = GATE.read().unwrap();
        let mut owned_child = Command::new("true").spawn().unwrap();
        OWNED.lock().unwrap().push(owned_child.id());
        drop(_gate);
        let stray = Command::new("true").spawn().unwrap();
        let stray_pid = stray.id();
        // Forgotten without a wait, as an adopted orphan is.
        std::mem::forget(stray);
        let deadline = Instant::now() + Duration::from_secs(10);
        while child_state(stray_pid, me) != ChildState::Zombie
            || child_state(owned_child.id(), me) != ChildState::Zombie
        {
            assert!(Instant::now() < deadline, "children never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
        let ours = |pid| pid == stray_pid || pid == owned_child.id();
        assert_eq!(reap_where(ours), 1);
        assert_eq!(child_state(stray_pid, me), ChildState::NotAChild);
        assert_eq!(child_state(owned_child.id(), me), ChildState::Zombie);
        // The owner still gets its own child's status.
        assert!(owned_child.wait().unwrap().success());
        reap_where(|_| false);
        assert!(!OWNED.lock().unwrap().contains(&owned_child.id()));
    }
}

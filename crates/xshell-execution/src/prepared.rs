//! Prepared file access for the read-only agent tools (findings F4, F5, F8).
//!
//! Authorization and acquisition must agree on the object they are about.
//! Resolving a path once to decide whether it is sensitive and again to read
//! it lets the object change in between: a probe swapped an ungated file for
//! a symlink to `.env` after the check and got the secret back. So a tool
//! call is *prepared* first: the path is walked from the root one component
//! at a time with `openat(2)`, never following a symlink implicitly, and the
//! walk ends holding an open descriptor. Sensitivity is decided from the path
//! that walk actually took, and the read uses that same descriptor, so what
//! was authorized is what is read.
//!
//! Semantics:
//!
//! - **Confinement.** Every component is opened relative to its parent's
//!   descriptor with `O_NOFOLLOW`. `..` never climbs above the root. A symlink
//!   is resolved by the walk itself; a relative target continues from the
//!   link's directory, and an absolute target must lie inside the root. A
//!   component replaced by a symlink mid-walk fails the open rather than being
//!   followed. Mount points inside the root are traversed like directories.
//! - **Identity.** The descriptor pins the object, so later renames or
//!   replacements of the path or its ancestors cannot change what is read.
//!   Writes to the same file *can* change its bytes: approval authorizes the
//!   object, not a snapshot of its content.
//! - **Hard links.** A hard link is a second name for the same file and is
//!   classified by the name used. Detecting every name of an inode is not
//!   possible in general, so a hard link to a credential under an innocent
//!   name is not gated.
//! - **Bounds.** A file read takes at most `max_bytes + 1` bytes from the
//!   descriptor; a listing stops reading entries after its limit, so memory
//!   does not grow with the size of the file or directory.
//!
//! The functions here block. Callers run them on a blocking worker with a
//! deadline (see `crate::tools`).

use crate::SensitivePaths;
use anyhow::{Context, Result, bail};
use std::collections::VecDeque;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

/// Symlinks followed in one walk before it is refused, as `ELOOP` would.
const MAX_SYMLINK_HOPS: usize = 40;

pub(crate) const OUTSIDE_ROOT: &str = "path is outside the current xshell working directory";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjectKind {
    File,
    Directory,
}

/// An object opened for a read-only tool and classified from the path the
/// walk actually took.
#[derive(Debug)]
pub(crate) struct PreparedObject {
    descriptor: OwnedFd,
    path: PathBuf,
    sensitive: bool,
}

impl PreparedObject {
    /// The canonical absolute path the walk resolved.
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn sensitive(&self) -> bool {
        self.sensitive
    }

    /// Read at most `max_bytes` from the prepared file, reporting whether more
    /// remained. Never reads more than `max_bytes + 1` bytes.
    pub(crate) fn read(self, max_bytes: usize) -> Result<(Vec<u8>, bool)> {
        let file = File::from(self.descriptor);
        let mut bytes = Vec::new();
        file.take(max_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("cannot read {}", self.path.display()))?;
        let truncated = bytes.len() > max_bytes;
        bytes.truncate(max_bytes);
        Ok((bytes, truncated))
    }

    /// Read at most `max_entries` entries of the prepared directory, in the
    /// order the filesystem returns them, reporting whether more remained.
    /// Each entry is a name and whether it is a directory.
    pub(crate) fn list(self, max_entries: usize) -> Result<(Vec<(OsString, bool)>, bool)> {
        // fdopendir takes ownership of the descriptor it is given.
        // SAFETY: dup(2) takes only an integer; `self.descriptor` is an open descriptor `self`
        // owns.
        let duplicate = unsafe { libc::dup(self.descriptor.as_raw_fd()) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("cannot list {}", self.path.display()));
        }
        // SAFETY: `duplicate` is a valid, owned directory descriptor.
        let stream = unsafe { libc::fdopendir(duplicate) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: fdopendir failed, so `duplicate` is still ours to close, and nothing else
            // uses it.
            unsafe { libc::close(duplicate) };
            return Err(error).with_context(|| format!("cannot list {}", self.path.display()));
        }
        let mut entries = Vec::new();
        let mut truncated = false;
        loop {
            // SAFETY: `stream` is a valid DIR*; each entry is read before the
            // next readdir call invalidates it.
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                break;
            }
            // SAFETY: `entry` is non-null and stays valid until the next readdir call; `d_name`
            // is NUL-terminated, and `name` is used only before that call.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            if entries.len() == max_entries {
                truncated = true;
                break;
            }
            // SAFETY: `entry` is non-null and valid until the next readdir call.
            let is_directory = match unsafe { (*entry).d_type } {
                libc::DT_DIR => true,
                // SAFETY: `stream` is a valid, open DIR*.
                libc::DT_UNKNOWN => stat_at(unsafe { libc::dirfd(stream) }, name)
                    .is_ok_and(|status| file_type(&status) == libc::S_IFDIR),
                _ => false,
            };
            entries.push((OsString::from_vec(bytes.to_vec()), is_directory));
        }
        // SAFETY: closes the stream and the duplicated descriptor it owns.
        unsafe { libc::closedir(stream) };
        Ok((entries, truncated))
    }
}

/// Open `requested` beneath `root` as an object of `kind`, without following
/// symlinks outside the walk, and classify it against `sensitive`.
pub(crate) fn prepare(
    root: &Path,
    requested: &str,
    kind: ObjectKind,
    sensitive: &SensitivePaths,
) -> Result<PreparedObject> {
    let spelled_root = root;
    let root = root
        .canonicalize()
        .with_context(|| format!("cannot resolve tool root {}", root.display()))?;
    let root_descriptor = open_at(libc::AT_FDCWD, root.as_os_str(), libc::O_DIRECTORY)
        .with_context(|| format!("cannot open tool root {}", root.display()))?;

    let mut pending = VecDeque::new();
    push_front_components(
        &mut pending,
        &relative_to_root(&root, spelled_root, Path::new(requested))?,
    );

    // The directories opened so far, root first, with the name each was
    // reached by. The path the walk took is these names joined.
    let mut opened: Vec<(OwnedFd, OsString)> = vec![(root_descriptor, OsString::new())];
    let mut hops = 0;
    while let Some(component) = pending.pop_front() {
        if component == ".." {
            if opened.len() == 1 {
                bail!(OUTSIDE_ROOT);
            }
            opened.pop();
            continue;
        }
        let parent = opened
            .last()
            .expect("the root is never popped")
            .0
            .as_raw_fd();
        let name = c_name(&component)?;
        let status = stat_at(parent, &name)
            .with_context(|| format!("cannot resolve {}", display(&root, &opened, &component)))?;
        if file_type(&status) == libc::S_IFLNK {
            hops += 1;
            if hops > MAX_SYMLINK_HOPS {
                bail!("too many symbolic links resolving {requested}");
            }
            let target = PathBuf::from(read_link_at(parent, &name).with_context(|| {
                format!("cannot read link {}", display(&root, &opened, &component))
            })?);
            if target.is_absolute() {
                // Absolute targets restart from the root and must stay in it.
                let inside = relative_to_root(&root, spelled_root, &target)?;
                opened.truncate(1);
                push_front_components(&mut pending, &inside);
            } else {
                push_front_components(&mut pending, &target);
            }
            continue;
        }
        // Intermediate components must be directories. O_NOFOLLOW makes a
        // component swapped for a symlink since the stat fail here instead of
        // being followed; O_NONBLOCK keeps a FIFO from blocking the open.
        let directory = if pending.is_empty() {
            0
        } else {
            libc::O_DIRECTORY
        };
        let descriptor = open_at(
            parent,
            &component,
            directory | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
        .with_context(|| format!("cannot open {}", display(&root, &opened, &component)))?;
        opened.push((descriptor, component));
    }

    let path = opened
        .iter()
        .skip(1)
        .fold(root.clone(), |path, (_, name)| path.join(name));
    let (descriptor, _) = opened.pop().expect("the root is always present");
    let status = stat_descriptor(descriptor.as_raw_fd())
        .with_context(|| format!("cannot inspect {}", path.display()))?;
    match (kind, file_type(&status)) {
        (ObjectKind::File, libc::S_IFREG) | (ObjectKind::Directory, libc::S_IFDIR) => {}
        (ObjectKind::File, _) => bail!("{} is not a regular file", path.display()),
        (ObjectKind::Directory, _) => bail!("{} is not a directory", path.display()),
    }
    let sensitive = sensitive.matches(&path);
    Ok(PreparedObject {
        descriptor,
        path,
        sensitive,
    })
}

/// `requested` as a path relative to the canonical `root`. Absolute paths,
/// whether requested or the target of a link, must name a location inside the
/// root, spelled canonically, as the caller spelled the root, or in any other
/// way that canonicalizes into it.
fn relative_to_root(root: &Path, spelled_root: &Path, requested: &Path) -> Result<PathBuf> {
    if !requested.is_absolute() {
        return Ok(requested.to_owned());
    }
    for prefix in [root, spelled_root] {
        if let Ok(inside) = requested.strip_prefix(prefix) {
            return Ok(inside.to_owned());
        }
    }
    // The root may be spelled differently (/tmp and /private/tmp on macOS).
    // Canonicalizing only chooses where the walk starts; the walk itself
    // still opens every component beneath the root descriptor.
    requested
        .canonicalize()
        .ok()
        .and_then(|canonical| canonical.strip_prefix(root).ok().map(Path::to_owned))
        .ok_or_else(|| anyhow::anyhow!(OUTSIDE_ROOT))
}

fn push_front_components(pending: &mut VecDeque<OsString>, path: &Path) {
    let components: Vec<OsString> = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_owned()),
            Component::ParentDir => Some(OsString::from("..")),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => None,
        })
        .collect();
    for component in components.into_iter().rev() {
        pending.push_front(component);
    }
}

fn display(root: &Path, opened: &[(OwnedFd, OsString)], component: &OsStr) -> String {
    opened
        .iter()
        .skip(1)
        .fold(root.to_owned(), |path, (_, name)| path.join(name))
        .join(component)
        .display()
        .to_string()
}

fn c_name(name: &OsStr) -> Result<CString> {
    CString::new(name.as_bytes()).context("path contains a NUL byte")
}

fn open_at(parent: RawFd, name: &OsStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    // SAFETY: `name` is a valid C string and `parent` a directory descriptor
    // or AT_FDCWD; the returned descriptor is owned below.
    let descriptor = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            flags | libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat succeeded, so `descriptor` is a new descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

fn stat_at(parent: RawFd, name: &CStr) -> io::Result<libc::stat> {
    let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `status` is written by fstatat on success.
    let result = unsafe {
        libc::fstatat(
            parent,
            name.as_ptr(),
            status.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstatat succeeded, so it initialized `status`.
    Ok(unsafe { status.assume_init() })
}

fn stat_descriptor(descriptor: RawFd) -> io::Result<libc::stat> {
    let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `status` is written by fstat on success.
    if unsafe { libc::fstat(descriptor, status.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstat succeeded, so it initialized `status`.
    Ok(unsafe { status.assume_init() })
}

fn read_link_at(parent: RawFd, name: &CStr) -> io::Result<OsString> {
    let mut buffer = vec![0_u8; libc::PATH_MAX as usize];
    // SAFETY: `buffer` has the length passed; readlinkat does not terminate.
    let length = unsafe {
        libc::readlinkat(
            parent,
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if length < 0 {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(length as usize);
    Ok(OsString::from_vec(buffer))
}

fn file_type(status: &libc::stat) -> libc::mode_t {
    status.st_mode & libc::S_IFMT
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn prepare_file(root: &Path, path: &str) -> Result<PreparedObject> {
        prepare(root, path, ObjectKind::File, &SensitivePaths::default())
    }

    fn read_all(object: PreparedObject) -> String {
        String::from_utf8(object.read(1 << 20).unwrap().0).unwrap()
    }

    fn workspace() -> (TempDir, PathBuf) {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path().join("root");
        fs::create_dir_all(root.join("dir")).unwrap();
        fs::write(root.join("notes.txt"), "notes").unwrap();
        fs::write(root.join("dir/notes.txt"), "nested notes").unwrap();
        fs::write(root.join(".env"), "SECRET=1").unwrap();
        fs::create_dir(temporary.path().join("outside")).unwrap();
        fs::write(temporary.path().join("outside/notes.txt"), "outside").unwrap();
        (temporary, root.canonicalize().unwrap())
    }

    #[test]
    fn f5_swapping_the_file_after_preparation_reads_the_prepared_object() {
        let (_temporary, root) = workspace();
        let prepared = prepare_file(&root, "notes.txt").unwrap();
        assert!(!prepared.sensitive());
        // Replace the authorized file with a symlink to the secret.
        fs::remove_file(root.join("notes.txt")).unwrap();
        symlink(root.join(".env"), root.join("notes.txt")).unwrap();
        assert_eq!(read_all(prepared), "notes");
    }

    #[test]
    fn f5_swapping_an_ancestor_after_preparation_reads_the_prepared_object() {
        let (temporary, root) = workspace();
        let prepared = prepare_file(&root, "dir/notes.txt").unwrap();
        // Move the directory away and put a link to an outside directory in
        // its place.
        fs::rename(root.join("dir"), root.join("moved")).unwrap();
        symlink(temporary.path().join("outside"), root.join("dir")).unwrap();
        assert_eq!(read_all(prepared), "nested notes");
        // Preparing again now refuses the outside link.
        let error = prepare_file(&root, "dir/notes.txt").unwrap_err();
        assert!(error.to_string().contains(OUTSIDE_ROOT), "{error:#}");
    }

    #[test]
    fn a_symlink_is_classified_by_its_target() {
        let (_temporary, root) = workspace();
        symlink(".env", root.join("innocent.txt")).unwrap();
        let prepared = prepare_file(&root, "innocent.txt").unwrap();
        assert!(prepared.sensitive());
        assert_eq!(prepared.path(), root.join(".env"));
    }

    #[test]
    fn confinement_holds_for_links_parents_and_absolute_paths() {
        let (temporary, root) = workspace();
        symlink("/etc", root.join("escape")).unwrap();
        symlink("../outside/notes.txt", root.join("up")).unwrap();
        symlink(root.join("notes.txt"), root.join("absolute-inside")).unwrap();
        for path in [
            "escape",
            "escape/hosts",
            "../outside/notes.txt",
            "up",
            "/etc/hosts",
        ] {
            let error = prepare_file(&root, path).unwrap_err();
            assert!(
                error.to_string().contains(OUTSIDE_ROOT),
                "{path}: {error:#}"
            );
        }
        let _ = temporary;
        // `..` that stays inside, and absolute links into the root, resolve.
        assert_eq!(
            read_all(prepare_file(&root, "dir/../notes.txt").unwrap()),
            "notes"
        );
        assert_eq!(
            read_all(prepare_file(&root, "absolute-inside").unwrap()),
            "notes"
        );
        let absolute = root.join("dir/notes.txt");
        assert_eq!(
            read_all(prepare_file(&root, absolute.to_str().unwrap()).unwrap()),
            "nested notes"
        );
    }

    /// macOS spells temporary directories through a link (`/var` is
    /// `/private/var`), so a link created with the root's own spelling has an
    /// absolute target that does not start with the canonical root.
    #[test]
    fn absolute_links_may_use_any_spelling_of_the_root() {
        let (temporary, root) = workspace();
        let spelled = temporary.path().join("spelled");
        symlink(&root, &spelled).unwrap();
        symlink(spelled.join(".env"), root.join("via-spelling")).unwrap();
        symlink(
            temporary.path().join("spelled/../outside/notes.txt"),
            root.join("via-spelling-up"),
        )
        .unwrap();

        for given_root in [&root, &spelled] {
            let prepared = prepare_file(given_root, "via-spelling").unwrap();
            assert!(prepared.sensitive());
            assert_eq!(prepared.path(), root.join(".env"));
            assert_eq!(read_all(prepared), "SECRET=1");
            let error = prepare_file(given_root, "via-spelling-up").unwrap_err();
            assert!(error.to_string().contains(OUTSIDE_ROOT), "{error:#}");
        }
    }

    #[test]
    fn symlink_loops_are_refused() {
        let (_temporary, root) = workspace();
        symlink("b", root.join("a")).unwrap();
        symlink("a", root.join("b")).unwrap();
        let error = prepare_file(&root, "a").unwrap_err();
        assert!(
            error.to_string().contains("too many symbolic links"),
            "{error:#}"
        );
    }

    #[test]
    fn kinds_are_checked_and_special_files_do_not_block() {
        let (_temporary, root) = workspace();
        assert!(prepare_file(&root, "dir").is_err());
        assert!(
            prepare(
                &root,
                "notes.txt",
                ObjectKind::Directory,
                &SensitivePaths::none()
            )
            .is_err()
        );
        // Opening a FIFO with no writer would block without O_NONBLOCK.
        let fifo = CString::new(root.join("pipe").as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let error = prepare_file(&root, "pipe").unwrap_err();
        assert!(
            error.to_string().contains("not a regular file"),
            "{error:#}"
        );
    }

    #[test]
    fn f8_reads_stop_one_byte_past_the_limit() {
        let (_temporary, root) = workspace();
        // A sparse 1 GiB file: reading it whole would allocate 1 GiB.
        let large = File::create(root.join("large.bin")).unwrap();
        large.set_len(1 << 30).unwrap();
        let (bytes, truncated) = prepare_file(&root, "large.bin").unwrap().read(1).unwrap();
        assert_eq!(bytes, [0]);
        assert!(truncated);
        let (bytes, truncated) = prepare_file(&root, "notes.txt").unwrap().read(5).unwrap();
        assert_eq!(bytes, b"notes");
        assert!(!truncated);
    }

    #[test]
    fn f8_listings_stop_at_the_limit() {
        let (_temporary, root) = workspace();
        let big = root.join("big");
        fs::create_dir(&big).unwrap();
        for index in 0..50 {
            fs::write(big.join(format!("file{index}")), "").unwrap();
        }
        fs::create_dir(big.join("subdir")).unwrap();
        let prepared =
            prepare(&root, "big", ObjectKind::Directory, &SensitivePaths::none()).unwrap();
        let (entries, truncated) = prepared.list(10).unwrap();
        assert_eq!(entries.len(), 10);
        assert!(truncated);
        let prepared =
            prepare(&root, "big", ObjectKind::Directory, &SensitivePaths::none()).unwrap();
        let (entries, truncated) = prepared.list(100).unwrap();
        assert_eq!(entries.len(), 51);
        assert!(!truncated);
        assert!(entries.contains(&(OsString::from("subdir"), true)));
        assert!(entries.contains(&(OsString::from("file0"), false)));
    }
}

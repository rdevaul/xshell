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
//! - **Letter case.** On a case-insensitive volume a name may be spelled in
//!   any case and still open the object (finding F13). The path recorded and
//!   classified uses the names the filesystem stores where the platform
//!   reports them (macOS, `F_GETPATH`), and sensitive-path matching folds
//!   case in any event, so `.ENV` is classified as `.env`.
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
        // Record the name the filesystem stores, not the one spelled: on a
        // case-insensitive volume `.ENV` opens `.env` (finding F13).
        let name = stored_name(parent, descriptor.as_raw_fd(), &component).unwrap_or(component);
        opened.push((descriptor, name));
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

/// The name under which `parent` stores the object `descriptor` opened as
/// `spelled`, when the platform can say and the answer is unambiguous. `None`
/// keeps the spelled name, which is then matched case-insensitively.
///
/// On macOS `fcntl(F_GETPATH)` returns the path with the case the volume
/// stores. A directory has one name, and so does a file with one link; a
/// file with several hard links reports whichever name the kernel has
/// cached, which may be another link than the one opened, so those keep the
/// spelled name. The answer is used only if it names, in `parent`, the same
/// object that is open, so a rename racing the walk cannot substitute a
/// name from elsewhere.
#[cfg(target_os = "macos")]
fn stored_name(parent: RawFd, descriptor: RawFd, spelled: &OsStr) -> Option<OsString> {
    let status = stat_descriptor(descriptor).ok()?;
    if file_type(&status) != libc::S_IFDIR && status.st_nlink != 1 {
        return None;
    }
    let mut buffer = vec![0_u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes a NUL-terminated path of at most PATH_MAX
    // bytes into the buffer, which has that length.
    if unsafe { libc::fcntl(descriptor, libc::F_GETPATH, buffer.as_mut_ptr()) } < 0 {
        return None;
    }
    let length = buffer.iter().position(|byte| *byte == 0)?;
    buffer.truncate(length);
    let stored = Path::new(OsStr::from_bytes(&buffer))
        .file_name()?
        .to_owned();
    if stored == spelled {
        return Some(stored);
    }
    let named = stat_at(parent, &c_name(&stored).ok()?).ok()?;
    (named.st_dev == status.st_dev && named.st_ino == status.st_ino).then_some(stored)
}

/// Other platforms keep the spelled name; matching folds case for them.
#[cfg(not(target_os = "macos"))]
fn stored_name(_parent: RawFd, _descriptor: RawFd, _spelled: &OsStr) -> Option<OsString> {
    None
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

/// Largest file `edit_file` reads in order to change it.
pub(crate) const MAX_EDIT_BYTES: usize = 8 << 20;

/// Where a write tool will write, resolved for authorization before anything
/// is created (the write counterpart of [`PreparedObject`]).
///
/// - The deepest directory on the path that exists is opened by the same
///   confined walk as a read ([`prepare`]), so its ancestors may be symlinks
///   that stay inside the root, and the path recorded is the one the walk
///   took.
/// - Directories below it that do not exist yet are only named here; they
///   are created by [`write`](Self::write), after approval, each opened with
///   `O_NOFOLLOW` so that one swapped for a symlink in the meantime fails.
/// - The file itself is never followed: a symlink at the final component is
///   refused, as is anything that is not a regular file.
#[derive(Debug)]
pub(crate) struct WriteTarget {
    directory: OwnedFd,
    missing: Vec<OsString>,
    name: OsString,
    path: PathBuf,
    sensitive: bool,
    exists: bool,
    must_exist: bool,
}

/// Resolve `requested` beneath `root` as a file to create or replace (or,
/// with `must_exist`, to change in place), and classify its path.
pub(crate) fn prepare_write(
    root: &Path,
    requested: &str,
    sensitive: &SensitivePaths,
    must_exist: bool,
) -> Result<WriteTarget> {
    if requested.trim().is_empty() {
        bail!("path is empty");
    }
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("cannot resolve tool root {}", root.display()))?;
    let relative = relative_to_root(&canonical_root, root, Path::new(requested))?;
    let mut components: Vec<OsString> = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => components.push(name.to_owned()),
            Component::CurDir => {}
            Component::ParentDir => components.push(OsString::from("..")),
            Component::RootDir | Component::Prefix(_) => bail!(OUTSIDE_ROOT),
        }
    }
    let Some(name) = components.pop().filter(|name| name != "..") else {
        bail!("{requested} does not name a file");
    };
    // The deepest existing directory: try the whole parent, then drop
    // trailing names that do not exist yet. `..` is only resolved within
    // existing directories, by the walk.
    let mut missing = Vec::new();
    let directory = loop {
        let parent: PathBuf = components.iter().collect();
        let spelled = if parent.as_os_str().is_empty() {
            ".".to_owned()
        } else {
            parent.to_string_lossy().into_owned()
        };
        match prepare(
            root,
            &spelled,
            ObjectKind::Directory,
            &SensitivePaths::none(),
        ) {
            Ok(directory) => break directory,
            Err(error) if is_not_found(&error) && !components.is_empty() => {
                let last = components.pop().expect("not empty");
                if last == ".." {
                    return Err(error);
                }
                missing.insert(0, last);
            }
            Err(error) => return Err(error),
        }
    };
    if must_exist && !missing.is_empty() {
        bail!("{requested} does not exist");
    }
    let path = missing
        .iter()
        .fold(directory.path.clone(), |path, name| path.join(name))
        .join(&name);
    let exists = if missing.is_empty() {
        match stat_at(directory.descriptor.as_raw_fd(), &c_name(&name)?) {
            Ok(status) => match file_type(&status) {
                libc::S_IFREG => true,
                libc::S_IFLNK => bail!(
                    "{} is a symbolic link; write to the file it points to instead",
                    path.display()
                ),
                _ => bail!("{} is not a regular file", path.display()),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(error).with_context(|| format!("cannot inspect {}", path.display()));
            }
        }
    } else {
        false
    };
    if must_exist && !exists {
        bail!("{} does not exist", path.display());
    }
    let sensitive = sensitive.matches(&path) || write_targets().matches(&path);
    Ok(WriteTarget {
        directory: directory.descriptor,
        missing,
        name,
        path,
        sensitive,
        exists,
        must_exist,
    })
}

/// The write-sensitive patterns, built once.
fn write_targets() -> &'static SensitivePaths {
    static TARGETS: std::sync::OnceLock<SensitivePaths> = std::sync::OnceLock::new();
    TARGETS.get_or_init(SensitivePaths::write_targets)
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<io::Error>())
        .any(|cause| cause.kind() == io::ErrorKind::NotFound)
}

impl WriteTarget {
    /// The canonical absolute path the file will have.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn sensitive(&self) -> bool {
        self.sensitive
    }

    /// Whether the file existed when the target was prepared.
    pub(crate) fn exists(&self) -> bool {
        self.exists
    }

    /// Whether the target was prepared for an edit of an existing file.
    pub(crate) fn must_exist(&self) -> bool {
        self.must_exist
    }

    /// Create the missing directories, then write `bytes` to a new file
    /// beside the target and move it into place. Returns whether the file was
    /// created.
    ///
    /// The target changes all at once or not at all: a failed or interrupted
    /// write leaves it as it was. A file the target named when it was
    /// prepared is replaced keeping its mode; when it did not exist, a file
    /// that appeared since (during an approval wait) is not replaced.
    pub(crate) fn write(self, bytes: &[u8]) -> Result<bool> {
        let directory = self.create_directories()?;
        let mode = if self.exists {
            match stat_at(directory.as_raw_fd(), &c_name(&self.name)?) {
                Ok(status) if file_type(&status) == libc::S_IFREG => Some(status.st_mode & 0o777),
                Ok(status) if file_type(&status) == libc::S_IFLNK => {
                    return Err(self.open_error(io::Error::from_raw_os_error(libc::ELOOP)));
                }
                Ok(_) => bail!("{} is not a regular file", self.path.display()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("cannot inspect {}", self.path.display()));
                }
            }
        } else {
            None
        };
        self.replace(&directory, bytes, mode, !self.exists)?;
        Ok(!self.exists)
    }

    /// Read the existing file (at most [`MAX_EDIT_BYTES`]), pass its text to
    /// `change`, and write what it returns in place of the file, as
    /// [`Self::write`] does.
    pub(crate) fn edit<T>(self, change: impl FnOnce(&str) -> Result<(String, T)>) -> Result<T> {
        let file = open_file_at(self.directory.as_raw_fd(), &self.name, libc::O_RDONLY)
            .map_err(|error| self.open_error(error))?;
        let mut file = self.regular(file)?;
        let mode = stat_descriptor(file.as_raw_fd())
            .with_context(|| format!("cannot inspect {}", self.path.display()))?
            .st_mode
            & 0o777;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_EDIT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("cannot read {}", self.path.display()))?;
        if bytes.len() > MAX_EDIT_BYTES {
            bail!(
                "{} is larger than the {MAX_EDIT_BYTES}-byte edit limit",
                self.path.display()
            );
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("{} is not UTF-8 text", self.path.display()))?;
        let (changed, outcome) = change(&text)?;
        drop(file);
        let directory = duplicate(&self.directory)?;
        self.replace(&directory, changed.as_bytes(), Some(mode), false)?;
        Ok(outcome)
    }

    /// Write `bytes` to a temporary file in `directory`, flush it to disk,
    /// and move it to the target's name: by `rename`, or with `exclusive`, by
    /// `link`, which fails if the name has appeared. The temporary file is
    /// removed on every path.
    fn replace(
        &self,
        directory: &OwnedFd,
        bytes: &[u8],
        mode: Option<libc::mode_t>,
        exclusive: bool,
    ) -> Result<()> {
        let parent = directory.as_raw_fd();
        let (temporary, file) = create_temporary(parent, &self.name)
            .with_context(|| format!("cannot create a file beside {}", self.path.display()))?;
        let removed = RemoveOnDrop(parent, temporary.clone());
        let mut file = File::from(file);
        // A new file has the usual 0666 less the umask, from `open`. A
        // replacement keeps the permission bits of the file it replaces, but
        // never set-id bits.
        if let Some(mode) = mode {
            // SAFETY: `file` is an open descriptor.
            if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
                return Err(io::Error::last_os_error())
                    .with_context(|| format!("cannot set the mode of {}", self.path.display()));
            }
        }
        std::io::Write::write_all(&mut file, bytes)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("cannot write {}", self.path.display()))?;
        drop(file);
        let from = c_name(&temporary)?;
        let to = c_name(&self.name)?;
        // SAFETY: both names are valid C strings and `parent` an open directory.
        let moved = unsafe {
            if exclusive {
                libc::linkat(parent, from.as_ptr(), parent, to.as_ptr(), 0)
            } else {
                libc::renameat(parent, from.as_ptr(), parent, to.as_ptr())
            }
        };
        if moved != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::AlreadyExists {
                bail!(
                    "{} was created by something else after this write was prepared; it was not \
replaced",
                    self.path.display()
                );
            }
            return Err(error).with_context(|| format!("cannot write {}", self.path.display()));
        }
        drop(removed);
        // SAFETY: `parent` is an open directory.
        unsafe { libc::fsync(parent) };
        Ok(())
    }

    fn create_directories(&self) -> Result<OwnedFd> {
        let mut directory = duplicate(&self.directory)?;
        for name in &self.missing {
            let c = c_name(name)?;
            // SAFETY: `c` is a valid C string; `directory` an open directory.
            let made = unsafe { libc::mkdirat(directory.as_raw_fd(), c.as_ptr(), 0o777) };
            if made != 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(error).with_context(|| {
                        format!("cannot create a directory for {}", self.path.display())
                    });
                }
            }
            directory = open_at(
                directory.as_raw_fd(),
                name,
                libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
            .with_context(|| format!("cannot open a directory for {}", self.path.display()))?;
        }
        Ok(directory)
    }

    fn open_error(&self, error: io::Error) -> anyhow::Error {
        if error.raw_os_error() == Some(libc::ELOOP) {
            anyhow::anyhow!(
                "{} is a symbolic link; write to the file it points to instead",
                self.path.display()
            )
        } else {
            anyhow::Error::new(error).context(format!("cannot open {}", self.path.display()))
        }
    }

    fn regular(&self, file: OwnedFd) -> Result<File> {
        let status = stat_descriptor(file.as_raw_fd())
            .with_context(|| format!("cannot inspect {}", self.path.display()))?;
        if file_type(&status) != libc::S_IFREG {
            bail!("{} is not a regular file", self.path.display());
        }
        Ok(File::from(file))
    }
}

fn duplicate(descriptor: &OwnedFd) -> Result<OwnedFd> {
    descriptor
        .try_clone()
        .context("cannot duplicate a directory descriptor")
}

/// Create a new, empty, private file beside `name` in `parent`, with a name
/// no other file has.
fn create_temporary(parent: RawFd, name: &OsStr) -> io::Result<(OsString, OwnedFd)> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        let mut temporary = OsString::from(".");
        temporary.push(name);
        temporary.push(format!(
            ".xshell-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match open_file_at(
            parent,
            &temporary,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        ) {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Removes the temporary file `name` in `parent` when dropped. After a
/// rename it names nothing, and the removal fails harmlessly; after a link
/// it removes the extra name.
struct RemoveOnDrop(RawFd, OsString);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Ok(name) = CString::new(self.1.as_bytes()) {
            // SAFETY: `name` is a valid C string and the directory is open.
            unsafe { libc::unlinkat(self.0, name.as_ptr(), 0) };
        }
    }
}

/// Open `name` in `parent` for writing, never following a symlink and never
/// blocking on a FIFO.
fn open_file_at(parent: RawFd, name: &OsStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    // SAFETY: `name` is a valid C string and `parent` a directory descriptor;
    // the returned descriptor is owned below.
    let descriptor = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            0o666 as libc::c_uint,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat succeeded, so `descriptor` is a new descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

#[cfg(test)]
pub(crate) mod tests {
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

    /// Whether `directory` is on a case-insensitive volume, by experiment.
    pub(crate) fn case_insensitive(directory: &Path) -> bool {
        let probe = directory.join("f13-case-probe");
        fs::write(&probe, "").unwrap();
        let insensitive = directory.join("F13-CASE-PROBE").exists();
        fs::remove_file(probe).unwrap();
        insensitive
    }

    /// F13: on a case-insensitive volume a name spelled in another case opens
    /// the stored object; it must be classified exactly as that object is.
    #[test]
    fn f13_case_variants_classify_like_the_stored_name() {
        let (_temporary, root) = workspace();
        if !case_insensitive(&root) {
            eprintln!(
                "skipping f13 filesystem test: {} is case-sensitive",
                root.display()
            );
            return;
        }
        fs::create_dir_all(root.join(".ssh")).unwrap();
        fs::write(root.join(".ssh/id_ed25519"), "key").unwrap();
        fs::create_dir_all(root.join("deploy")).unwrap();
        fs::write(root.join("deploy/server.pem"), "pem").unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "token").unwrap();
        for (variant, stored) in [
            (".ENV", ".env"),
            (".SSH/ID_ED25519", ".ssh/id_ed25519"),
            ("deploy/server.PEM", "deploy/server.pem"),
            ("DEPLOY/SERVER.PEM", "deploy/server.pem"),
            (".GIT/config", ".git/config"),
            (".Git/CONFIG", ".git/config"),
        ] {
            let canonical = prepare_file(&root, stored).unwrap();
            assert!(canonical.sensitive(), "{stored} is sensitive");
            let prepared = prepare_file(&root, variant).unwrap();
            assert!(
                prepared.sensitive(),
                "f13: {variant} must be sensitive like {stored}"
            );
            // macOS reports the stored names, so the recorded path is the
            // object's own path however it was spelled.
            #[cfg(target_os = "macos")]
            assert_eq!(prepared.path(), root.join(stored), "f13: {variant}");
        }
        let directory = prepare(
            &root,
            ".SSH",
            ObjectKind::Directory,
            &SensitivePaths::default(),
        )
        .unwrap();
        assert!(directory.sensitive(), "f13: .SSH must be sensitive");
        // A hard link keeps the spelled name (the kernel may report another
        // link's), and folding still classifies it.
        fs::hard_link(root.join(".env"), root.join("linked.txt")).unwrap();
        let linked = prepare_file(&root, ".ENV").unwrap();
        assert!(linked.sensitive(), "f13: a hard-linked .ENV is sensitive");
        // Ordinary files keep classifying as before.
        assert!(!prepare_file(&root, "NOTES.TXT").unwrap().sensitive());
    }

    /// F13 on macOS: APFS folds more than ASCII case. These spellings open
    /// `server.key` there, and the stored name is what gets classified.
    #[cfg(target_os = "macos")]
    #[test]
    fn f13_unicode_foldings_classify_like_the_stored_name() {
        let (_temporary, root) = workspace();
        if !case_insensitive(&root) {
            eprintln!(
                "skipping f13 filesystem test: {} is case-sensitive",
                root.display()
            );
            return;
        }
        fs::write(root.join("server.key"), "key").unwrap();
        for variant in ["\u{17F}erver.key", "server.\u{212A}ey", "SERVER.KEY"] {
            let Ok(prepared) = prepare_file(&root, variant) else {
                // This volume does not fold this spelling; nothing to dodge.
                continue;
            };
            assert!(prepared.sensitive(), "f13: {variant:?} must be sensitive");
            assert_eq!(prepared.path(), root.join("server.key"), "f13: {variant:?}");
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

    fn target(root: &Path, path: &str) -> Result<WriteTarget> {
        prepare_write(root, path, &SensitivePaths::new([".env".to_owned()]), false)
    }

    #[test]
    fn writes_create_missing_directories_only_when_written() {
        let (_temporary, root) = workspace();
        let prepared = target(&root, "new/deep/file.txt").unwrap();
        assert_eq!(prepared.path(), root.join("new/deep/file.txt"));
        assert!(!prepared.exists());
        // Preparing creates nothing: approval comes first.
        assert!(!root.join("new").exists());
        assert!(prepared.write(b"hello").unwrap());
        assert_eq!(
            fs::read_to_string(root.join("new/deep/file.txt")).unwrap(),
            "hello"
        );
        // Overwriting reports an existing file and truncates it.
        let again = target(&root, "new/deep/file.txt").unwrap();
        assert!(again.exists());
        assert!(!again.write(b"hi").unwrap());
        assert_eq!(
            fs::read_to_string(root.join("new/deep/file.txt")).unwrap(),
            "hi"
        );
    }

    #[test]
    fn writes_stay_inside_the_root() {
        let (temporary, root) = workspace();
        for escape in [
            "../outside/notes.txt",
            "dir/../../outside/x.txt",
            "/etc/xshell-test",
        ] {
            let error = target(&root, escape).unwrap_err().to_string();
            assert!(error.contains("outside"), "{escape}: {error}");
        }
        // A directory symlink that leaves the root is refused by the walk.
        symlink(temporary.path().join("outside"), root.join("out")).unwrap();
        assert!(target(&root, "out/x.txt").is_err());
        assert!(!temporary.path().join("outside/x.txt").exists());
        // One that stays inside is followed.
        symlink(root.join("dir"), root.join("inside")).unwrap();
        let prepared = target(&root, "inside/x.txt").unwrap();
        assert_eq!(prepared.path(), root.join("dir/x.txt"));
        prepared.write(b"x").unwrap();
        assert_eq!(fs::read_to_string(root.join("dir/x.txt")).unwrap(), "x");
    }

    #[test]
    fn a_symlink_at_the_file_is_never_written_through() {
        let (temporary, root) = workspace();
        symlink(
            temporary.path().join("outside/notes.txt"),
            root.join("link.txt"),
        )
        .unwrap();
        let error = target(&root, "link.txt").unwrap_err().to_string();
        assert!(error.contains("symbolic link"), "{error}");
        assert_eq!(
            fs::read_to_string(temporary.path().join("outside/notes.txt")).unwrap(),
            "outside"
        );
        // Swapped in after preparation: a new file is linked into place only
        // where nothing is, so the link is neither followed nor replaced.
        let prepared = target(&root, "late.txt").unwrap();
        symlink(
            temporary.path().join("outside/notes.txt"),
            root.join("late.txt"),
        )
        .unwrap();
        let error = prepared.write(b"x").unwrap_err().to_string();
        assert!(error.contains("not replaced"), "{error}");
        assert!(
            fs::symlink_metadata(root.join("late.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(temporary.path().join("outside/notes.txt")).unwrap(),
            "outside"
        );
    }

    #[test]
    fn a_replacement_is_atomic_keeps_the_mode_and_leaves_hard_links_alone() {
        use std::os::unix::fs::PermissionsExt;
        let (_temporary, root) = workspace();
        fs::write(root.join("run.sh"), "old").unwrap();
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o751)).unwrap();
        fs::hard_link(root.join("run.sh"), root.join("other-name")).unwrap();
        target(&root, "run.sh").unwrap().write(b"new").unwrap();
        assert_eq!(fs::read_to_string(root.join("run.sh")).unwrap(), "new");
        assert_eq!(
            fs::metadata(root.join("run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o751
        );
        // The other name still has the old content: the write replaced the
        // directory entry, not the shared file.
        assert_eq!(fs::read_to_string(root.join("other-name")).unwrap(), "old");
        // No temporary file is left behind.
        let names: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.contains(".xshell-"))
            .collect();
        assert!(names.is_empty(), "{names:?}");
    }

    #[test]
    fn a_file_that_appears_after_preparation_is_not_replaced() {
        let (_temporary, root) = workspace();
        let prepared = target(&root, "new.txt").unwrap();
        assert!(!prepared.exists());
        fs::write(root.join("new.txt"), "someone else's").unwrap();
        let error = prepared.write(b"mine").unwrap_err().to_string();
        assert!(error.contains("not replaced"), "{error}");
        assert_eq!(
            fs::read_to_string(root.join("new.txt")).unwrap(),
            "someone else's"
        );
    }

    #[test]
    fn writes_to_start_up_files_and_hooks_are_sensitive() {
        let (_temporary, root) = workspace();
        for path in [".zshenv", ".bashrc", ".git/hooks/pre-commit", ".gitconfig"] {
            let prepared = prepare_write(&root, path, &SensitivePaths::none(), false).unwrap();
            assert!(prepared.sensitive(), "{path}");
        }
        let plain = prepare_write(&root, "src/main.rs", &SensitivePaths::none(), false).unwrap();
        assert!(!plain.sensitive());
    }

    #[test]
    fn write_targets_are_classified_and_checked() {
        let (_temporary, root) = workspace();
        assert!(target(&root, ".env").unwrap().sensitive());
        assert!(!target(&root, "notes.txt").unwrap().sensitive());
        assert!(
            target(&root, "dir")
                .unwrap_err()
                .to_string()
                .contains("not a regular file")
        );
        assert!(target(&root, "notes.txt/x").is_err());
        assert!(target(&root, "").is_err());
        assert!(target(&root, "dir/..").is_err());
        let missing = prepare_write(&root, "nope.txt", &SensitivePaths::none(), true);
        assert!(missing.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn edits_change_the_file_in_place() {
        let (_temporary, root) = workspace();
        let prepared =
            prepare_write(&root, "dir/notes.txt", &SensitivePaths::none(), true).unwrap();
        let count = prepared
            .edit(|text| Ok((text.replace("nested", "edited"), 1)))
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            fs::read_to_string(root.join("dir/notes.txt")).unwrap(),
            "edited notes"
        );
        // A shorter result leaves no trailing bytes.
        let prepared =
            prepare_write(&root, "dir/notes.txt", &SensitivePaths::none(), true).unwrap();
        prepared.edit(|_| Ok(("x".to_owned(), ()))).unwrap();
        assert_eq!(fs::read_to_string(root.join("dir/notes.txt")).unwrap(), "x");
        // A refused change leaves the file alone.
        let prepared =
            prepare_write(&root, "dir/notes.txt", &SensitivePaths::none(), true).unwrap();
        assert!(prepared.edit::<()>(|_| bail!("no match")).is_err());
        assert_eq!(fs::read_to_string(root.join("dir/notes.txt")).unwrap(), "x");
    }
}

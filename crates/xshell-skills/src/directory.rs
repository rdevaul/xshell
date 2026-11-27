//! Checking a directory against its manifest.
//!
//! The check describes the directory at the moment it runs. A caller that
//! will use the files afterwards has to own the directory, so nothing can
//! change between the check and the use: the store copies a skill into a
//! directory only the daemon can write and checks the copy.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::SkillError;
use crate::manifest::{MANIFEST_NAME, MAX_FILES, Manifest, SIGNATURE_NAME};

fn directory(message: String) -> SkillError {
    SkillError::Directory(message)
}

/// Open a file that must be a regular file, not a link to one.
fn open_regular(path: &Path, shown: &str) -> Result<(File, u64), SkillError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| directory(format!("cannot open {shown}: {error}")))?;
    let metadata = file
        .metadata()
        .map_err(|error| directory(format!("cannot read {shown}: {error}")))?;
    if !metadata.is_file() {
        return Err(directory(format!("{shown} is not a regular file")));
    }
    Ok((file, metadata.len()))
}

/// Read one of the two manifest files at the top of a skill.
pub(crate) fn read_top_file(dir: &Path, name: &str, limit: u64) -> Result<Vec<u8>, SkillError> {
    let (file, _) = open_regular(&dir.join(name), name)?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| directory(format!("cannot read {name}: {error}")))?;
    if bytes.len() as u64 > limit {
        return Err(directory(format!("{name} is larger than {limit} bytes")));
    }
    Ok(bytes)
}

/// Every regular file under `dir`, as `/`-separated relative paths, without
/// the two manifest files. Anything that is not a regular file or a
/// directory is an error.
fn list_files(dir: &Path) -> Result<BTreeSet<String>, SkillError> {
    let top = fs::symlink_metadata(dir)
        .map_err(|error| directory(format!("cannot read {}: {error}", dir.display())))?;
    if !top.is_dir() {
        return Err(directory(format!("{} is not a directory", dir.display())));
    }
    let mut files = BTreeSet::new();
    let mut pending = vec![String::new()];
    while let Some(relative) = pending.pop() {
        let entries = fs::read_dir(dir.join(&relative))
            .map_err(|error| directory(format!("cannot list {relative:?}: {error}")))?;
        for entry in entries {
            let entry =
                entry.map_err(|error| directory(format!("cannot list {relative:?}: {error}")))?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| directory(format!("a file name in {relative:?} is not UTF-8")))?;
            let path = if relative.is_empty() {
                name.to_owned()
            } else {
                format!("{relative}/{name}")
            };
            // `DirEntry::file_type` does not follow links.
            let kind = entry
                .file_type()
                .map_err(|error| directory(format!("cannot read {path:?}: {error}")))?;
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_symlink() {
                return Err(directory(format!("{path:?} is a symbolic link")));
            } else if !kind.is_file() {
                return Err(directory(format!("{path:?} is not a regular file")));
            } else if path != MANIFEST_NAME && path != SIGNATURE_NAME {
                // The manifest lists at most MAX_FILES, so one more than
                // that is already a failure; stop before walking a huge tree.
                if files.len() > MAX_FILES {
                    return Err(directory(format!("more than {MAX_FILES} files")));
                }
                files.insert(path);
            }
        }
    }
    Ok(files)
}

/// The directory must hold exactly the listed files, each with the listed
/// size and SHA-256, and nothing that is not a regular file or a directory.
pub(crate) fn check(dir: &Path, manifest: &Manifest) -> Result<(), SkillError> {
    let present = list_files(dir)?;
    if let Some(unlisted) = present
        .iter()
        .find(|path| !manifest.files.contains_key(*path))
    {
        return Err(directory(format!(
            "{unlisted:?} is not listed in the manifest"
        )));
    }
    if let Some(missing) = manifest.files.keys().find(|path| !present.contains(*path)) {
        return Err(directory(format!("listed file {missing:?} is missing")));
    }
    for (path, entry) in &manifest.files {
        let (file, size) = open_regular(&dir.join(path), path)?;
        if size != entry.size {
            return Err(directory(format!(
                "{path:?} is {size} bytes; the manifest says {}",
                entry.size
            )));
        }
        // Read no more than the listed size plus one byte, so a file that
        // grows while it is read cannot make this read without bound.
        let mut hasher = Sha256::new();
        let mut reader = file.take(entry.size + 1);
        let read = std::io::copy(&mut reader, &mut hasher)
            .map_err(|error| directory(format!("cannot read {path:?}: {error}")))?;
        if read != entry.size || hex::encode(hasher.finalize()) != entry.sha256 {
            return Err(directory(format!("{path:?} does not match the manifest")));
        }
    }
    Ok(())
}

//! Content-addressed storage for a session's images.
//!
//! Messages carry [`ImageRef`]s; the bytes live here, one file per image,
//! named by the SHA-256 of its contents. Keeping bytes out of messages keeps
//! them out of `sessions.json`, the session protocol, and the event journal,
//! and makes a repeated render free.
//!
//! Each session has its own store directory. A session can therefore only
//! use images that were stored for it, never another session's image by
//! guessing its hash, and closing a session removes its images with it.
//!
//! The store holds bytes that ingestion has already decoded, bounded, and
//! re-encoded; it does not decode anything itself. It does verify every read
//! against the reference's hash and length, so a file changed on disk is
//! refused rather than sent to a provider.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use thiserror::Error;
use xshell_core::{ImageMediaType, ImageRef};

/// Bounds on one session's store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageStoreLimits {
    /// Largest single stored image, in bytes.
    pub max_image_bytes: u64,
    /// Most bytes one session may hold.
    pub max_session_bytes: u64,
    /// Most images one session may hold.
    pub max_session_images: usize,
}

impl ImageStoreLimits {
    /// 20 MiB: far above a re-encoded image at the default maximum edge.
    pub const DEFAULT_MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
    /// 512 MiB per session.
    pub const DEFAULT_MAX_SESSION_BYTES: u64 = 512 * 1024 * 1024;
    pub const DEFAULT_MAX_SESSION_IMAGES: usize = 2048;
}

impl Default for ImageStoreLimits {
    fn default() -> Self {
        Self {
            max_image_bytes: Self::DEFAULT_MAX_IMAGE_BYTES,
            max_session_bytes: Self::DEFAULT_MAX_SESSION_BYTES,
            max_session_images: Self::DEFAULT_MAX_SESSION_IMAGES,
        }
    }
}

/// Why the store refused an image. Separate from I/O failures so callers can
/// tell the model or the user what to do.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ImageStoreError {
    #[error("the image is {bytes} bytes; the limit is {limit}")]
    ImageTooLarge { bytes: u64, limit: u64 },
    #[error("the image is empty")]
    Empty,
    #[error(
        "the session's image store is full ({images} images, {bytes} bytes); \
         remove images from the conversation or start a new session"
    )]
    SessionFull { images: usize, bytes: u64 },
    #[error("image {0} is not in this session's store")]
    Missing(String),
    #[error("image {0} in the store does not match its reference")]
    Corrupt(String),
}

/// What one collection pass removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CollectionReport {
    pub removed_images: usize,
    pub removed_bytes: u64,
}

/// Current use of one session's store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreUsage {
    pub images: usize,
    pub bytes: u64,
}

/// One session's image store.
#[derive(Debug, Clone)]
pub struct ImageStore {
    directory: PathBuf,
    limits: ImageStoreLimits,
}

impl ImageStore {
    /// The store for `session_id` under `root` (normally the daemon's
    /// `images` state directory), created if needed.
    pub fn open(root: &Path, session_id: &str, limits: ImageStoreLimits) -> Result<Self> {
        validate_session_id(session_id)?;
        xshell_platform::ensure_secure_directory(root, "image store")?;
        let directory = root.join(session_id);
        xshell_platform::ensure_secure_directory(&directory, "session image store")?;
        Ok(Self { directory, limits })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn limits(&self) -> ImageStoreLimits {
        self.limits
    }

    /// Store re-encoded image bytes and return their reference. The caller
    /// (ingestion) supplies the dimensions it decoded. Storing bytes that
    /// are already present returns a reference without writing again and
    /// without counting against the limits a second time.
    ///
    /// Concurrent writers to one session may together pass the session
    /// limits by at most one image each; the limits bound growth, not exact
    /// accounting.
    pub fn put(
        &self,
        bytes: &[u8],
        media_type: ImageMediaType,
        width: u32,
        height: u32,
        label: &str,
    ) -> Result<ImageRef> {
        let length = bytes.len() as u64;
        if length == 0 {
            return Err(ImageStoreError::Empty.into());
        }
        if length > self.limits.max_image_bytes {
            return Err(ImageStoreError::ImageTooLarge {
                bytes: length,
                limit: self.limits.max_image_bytes,
            }
            .into());
        }
        let sha256 = hex::encode(Sha256::digest(bytes));
        let image = ImageRef::new(&sha256, media_type, width, height, length, label)
            .context("invalid image reference")?;
        let path = self.directory.join(&sha256);
        if self.verify_file(&path, &sha256, length).is_ok() {
            return Ok(image);
        }

        let usage = self.usage()?;
        if usage.images >= self.limits.max_session_images
            || usage.bytes.saturating_add(length) > self.limits.max_session_bytes
        {
            return Err(ImageStoreError::SessionFull {
                images: usage.images,
                bytes: usage.bytes,
            }
            .into());
        }

        let temporary = self
            .directory
            .join(format!(".{sha256}.{}.tmp", uuid_like_suffix()));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&temporary)
                .with_context(|| format!("cannot create {}", temporary.display()))?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &path)
                .with_context(|| format!("cannot store image {}", path.display()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        Ok(image)
    }

    /// The bytes of `image`, verified against its hash and length.
    pub fn get(&self, image: &ImageRef) -> Result<Vec<u8>> {
        let path = self.directory.join(image.sha256());
        let mut file = match open_regular(&path) {
            Ok(file) => file,
            Err(error) if is_not_found(&error) => {
                return Err(ImageStoreError::Missing(image.sha256().to_owned()).into());
            }
            Err(error) => return Err(error),
        };
        let corrupt = || ImageStoreError::Corrupt(image.sha256().to_owned());
        if file.metadata()?.len() != image.byte_len() {
            return Err(corrupt().into());
        }
        let mut bytes = Vec::with_capacity(image.byte_len() as usize);
        // Read one byte past the expected length so a file that grew after
        // the check is caught too.
        Read::by_ref(&mut file)
            .take(image.byte_len().saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != image.byte_len()
            || hex::encode(Sha256::digest(&bytes)) != image.sha256()
        {
            return Err(corrupt().into());
        }
        Ok(bytes)
    }

    /// Whether the store holds `image`. Checks presence and length only; use
    /// [`get`](Self::get) for a verified read.
    pub fn contains(&self, image: &ImageRef) -> bool {
        fs::symlink_metadata(self.directory.join(image.sha256()))
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() == image.byte_len())
    }

    /// Fail unless every image in `images` is in the store. Used to refuse
    /// history from a client that references images this session never
    /// stored.
    pub fn require_all<'a>(&self, images: impl IntoIterator<Item = &'a ImageRef>) -> Result<()> {
        for image in images {
            if !self.contains(image) {
                return Err(ImageStoreError::Missing(image.sha256().to_owned()).into());
            }
        }
        Ok(())
    }

    pub fn usage(&self) -> Result<StoreUsage> {
        let mut usage = StoreUsage::default();
        for entry in self.entries()? {
            let (_, metadata) = entry?;
            usage.images += 1;
            usage.bytes = usage.bytes.saturating_add(metadata.len());
        }
        Ok(usage)
    }

    /// Remove every stored image whose hash is not in `referenced`, and any
    /// stray files a crashed write left behind.
    pub fn retain<'a>(
        &self,
        referenced: impl IntoIterator<Item = &'a ImageRef>,
    ) -> Result<CollectionReport> {
        let keep: HashSet<&str> = referenced.into_iter().map(ImageRef::sha256).collect();
        let mut report = CollectionReport::default();
        for entry in fs::read_dir(&self.directory)
            .with_context(|| format!("cannot read {}", self.directory.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if is_hash(&name) && keep.contains(name.as_ref()) {
                continue;
            }
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                // Nothing the store writes is a directory; leave it for a
                // person to look at rather than delete recursively.
                continue;
            }
            fs::remove_file(entry.path())
                .with_context(|| format!("cannot remove {}", entry.path().display()))?;
            if is_hash(&name) {
                report.removed_images += 1;
                report.removed_bytes = report.removed_bytes.saturating_add(metadata.len());
            }
        }
        Ok(report)
    }

    /// Delete the store and everything in it, when its session ends.
    pub fn remove(self) -> Result<()> {
        remove_store_directory(&self.directory)
    }

    /// Stored images, as (hash, metadata). Stray files are skipped.
    fn entries(&self) -> Result<impl Iterator<Item = Result<(String, fs::Metadata)>>> {
        let entries = fs::read_dir(&self.directory)
            .with_context(|| format!("cannot read {}", self.directory.display()))?;
        Ok(entries.filter_map(|entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => return Some(Err(error.into())),
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_hash(&name) {
                return None;
            }
            match entry.metadata() {
                Ok(metadata) if metadata.is_file() => Some(Ok((name, metadata))),
                Ok(_) => None,
                Err(error) => Some(Err(error.into())),
            }
        }))
    }

    fn verify_file(&self, path: &Path, sha256: &str, length: u64) -> Result<()> {
        let mut file = open_regular(path)?;
        if file.metadata()?.len() != length {
            bail!("length mismatch");
        }
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher)?;
        if hex::encode(hasher.finalize()) != sha256 {
            bail!("hash mismatch");
        }
        Ok(())
    }
}

/// Remove the stores under `root` that belong to no session in `live`:
/// ephemeral sessions that ended while the daemon was down, or sessions
/// whose removal failed part-way. Returns how many stores were removed.
pub fn remove_orphan_stores<'a>(
    root: &Path,
    live: impl IntoIterator<Item = &'a str>,
) -> Result<usize> {
    if !root.exists() {
        return Ok(0);
    }
    xshell_platform::ensure_secure_directory(root, "image store")?;
    let live: HashSet<&str> = live.into_iter().collect();
    let mut removed = 0;
    for entry in fs::read_dir(root).with_context(|| format!("cannot read {}", root.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if live.contains(name.as_ref()) || validate_session_id(&name).is_err() {
            continue;
        }
        if entry.file_type()?.is_dir() {
            remove_store_directory(&entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Remove one session's store directory: its files, then the directory.
/// The store never creates subdirectories, so this is not recursive, and a
/// directory with anything unexpected in it is left in place with an error.
fn remove_store_directory(directory: &Path) -> Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => bail!("{} is not a directory", directory.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            bail!(
                "unexpected directory {} in an image store",
                entry.path().display()
            );
        }
        fs::remove_file(entry.path())?;
    }
    fs::remove_dir(directory)
        .with_context(|| format!("cannot remove image store {}", directory.display()))
}

fn validate_session_id(session_id: &str) -> Result<()> {
    // Session IDs are UUIDs. Checking the shape keeps a malformed ID from
    // naming a path outside the store root.
    let valid = session_id.len() == 36
        && session_id.char_indices().all(|(index, c)| match index {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    if !valid {
        bail!("invalid session ID {session_id:?} for an image store");
    }
    Ok(())
}

fn is_hash(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn open_regular(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        bail!("{} is not a plain stored image", path.display());
    }
    Ok(file)
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

/// A suffix unique enough to keep concurrent temporary files apart.
fn uuid_like_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!(
        "{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const SESSION: &str = "0b1c6d52-7a43-4e43-9a1e-2f1f4c2d7e10";
    const OTHER: &str = "9f0e1d2c-3b4a-4596-8778-695a4b3c2d1e";

    fn root() -> (TempDir, PathBuf) {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("images");
        (temp, root)
    }

    fn store(root: &Path, limits: ImageStoreLimits) -> ImageStore {
        ImageStore::open(root, SESSION, limits).unwrap()
    }

    fn put(store: &ImageStore, bytes: &[u8]) -> Result<ImageRef> {
        store.put(bytes, ImageMediaType::Png, 4, 3, "plot.png")
    }

    fn store_error(error: &anyhow::Error) -> &ImageStoreError {
        error.downcast_ref::<ImageStoreError>().unwrap()
    }

    #[test]
    fn stores_and_returns_verified_bytes() {
        let (_temp, root) = root();
        let store = store(&root, ImageStoreLimits::default());
        let image = put(&store, b"not really a png").unwrap();
        assert_eq!(image.byte_len(), 16);
        assert_eq!(image.label(), "plot.png");
        assert_eq!(
            image.sha256(),
            hex::encode(Sha256::digest(b"not really a png"))
        );
        assert!(store.contains(&image));
        assert_eq!(store.get(&image).unwrap(), b"not really a png");

        let mode = fs::metadata(store.directory().join(image.sha256()))
            .unwrap()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn identical_bytes_are_stored_once() {
        let (_temp, root) = root();
        let limits = ImageStoreLimits {
            max_session_images: 1,
            ..ImageStoreLimits::default()
        };
        let store = store(&root, limits);
        let first = put(&store, b"same").unwrap();
        let second = store
            .put(b"same", ImageMediaType::Png, 4, 3, "again.png")
            .unwrap();
        assert_eq!(first.sha256(), second.sha256());
        assert_eq!(second.label(), "again.png");
        assert_eq!(
            store.usage().unwrap(),
            StoreUsage {
                images: 1,
                bytes: 4
            }
        );
    }

    #[test]
    fn enforces_image_and_session_limits() {
        let (_temp, root) = root();
        let limits = ImageStoreLimits {
            max_image_bytes: 8,
            max_session_bytes: 12,
            max_session_images: 2,
        };
        let store = store(&root, limits);
        let error = put(&store, b"123456789").unwrap_err();
        assert_eq!(
            store_error(&error),
            &ImageStoreError::ImageTooLarge { bytes: 9, limit: 8 }
        );
        assert_eq!(
            store_error(&put(&store, b"").unwrap_err()),
            &ImageStoreError::Empty
        );

        put(&store, b"12345678").unwrap();
        let error = put(&store, b"abcde").unwrap_err();
        assert!(matches!(
            store_error(&error),
            ImageStoreError::SessionFull {
                images: 1,
                bytes: 8
            }
        ));
        put(&store, b"abcd").unwrap();
        let error = put(&store, b"x").unwrap_err();
        assert!(matches!(
            store_error(&error),
            ImageStoreError::SessionFull { images: 2, .. }
        ));
    }

    #[test]
    fn refuses_bytes_changed_on_disk() {
        let (_temp, root) = root();
        let store = store(&root, ImageStoreLimits::default());
        let image = put(&store, b"original").unwrap();
        let path = store.directory().join(image.sha256());
        fs::write(&path, b"tampered").unwrap();
        assert_eq!(
            store_error(&store.get(&image).unwrap_err()),
            &ImageStoreError::Corrupt(image.sha256().into())
        );
        fs::write(&path, b"longer than before").unwrap();
        assert!(store.get(&image).is_err());

        // Storing the original again repairs the entry.
        put(&store, b"original").unwrap();
        assert_eq!(store.get(&image).unwrap(), b"original");
    }

    #[test]
    fn refuses_links_in_place_of_images() {
        let (temp, root) = root();
        let store = store(&root, ImageStoreLimits::default());
        let image = put(&store, b"original").unwrap();
        let path = store.directory().join(image.sha256());
        fs::remove_file(&path).unwrap();
        let outside = temp.path().join("secret");
        fs::write(&outside, b"original").unwrap();

        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(store.get(&image).is_err());
        assert!(!store.contains(&image));

        fs::remove_file(&path).unwrap();
        fs::hard_link(&outside, &path).unwrap();
        assert!(store.get(&image).is_err());
    }

    #[test]
    fn sessions_cannot_read_each_others_images() {
        let (_temp, root) = root();
        let first = store(&root, ImageStoreLimits::default());
        let image = put(&first, b"private render").unwrap();
        let second = ImageStore::open(&root, OTHER, ImageStoreLimits::default()).unwrap();
        assert!(!second.contains(&image));
        assert_eq!(
            store_error(&second.get(&image).unwrap_err()),
            &ImageStoreError::Missing(image.sha256().into())
        );
        assert!(second.require_all([&image]).is_err());
        assert!(first.require_all([&image]).is_ok());
    }

    #[test]
    fn retain_removes_unreferenced_images_and_stray_files() {
        let (_temp, root) = root();
        let store = store(&root, ImageStoreLimits::default());
        let kept = put(&store, b"kept").unwrap();
        let dropped = put(&store, b"dropped").unwrap();
        fs::write(store.directory().join(".abc.tmp"), b"partial").unwrap();

        let report = store.retain([&kept]).unwrap();
        assert_eq!(
            report,
            CollectionReport {
                removed_images: 1,
                removed_bytes: 7
            }
        );
        assert!(store.contains(&kept));
        assert!(!store.contains(&dropped));
        assert_eq!(fs::read_dir(store.directory()).unwrap().count(), 1);
    }

    #[test]
    fn removing_a_store_and_orphans() {
        let (_temp, root) = root();
        let live = store(&root, ImageStoreLimits::default());
        put(&live, b"live").unwrap();
        let orphan = ImageStore::open(&root, OTHER, ImageStoreLimits::default()).unwrap();
        put(&orphan, b"orphan").unwrap();
        fs::create_dir(root.join("not-a-session")).unwrap();

        assert_eq!(remove_orphan_stores(&root, [SESSION]).unwrap(), 1);
        assert!(live.directory().exists());
        assert!(!root.join(OTHER).exists());
        assert!(
            root.join("not-a-session").exists(),
            "only session stores are removed"
        );

        let directory = live.directory().to_owned();
        live.remove().unwrap();
        assert!(!directory.exists());
        assert_eq!(remove_orphan_stores(&root.join("absent"), []).unwrap(), 0);
    }

    #[test]
    fn rejects_malformed_session_ids() {
        let (_temp, root) = root();
        for id in [
            "",
            "..",
            "../escape",
            "0b1c6d52-7a43-4e43-9a1e-2f1f4c2d7e1/",
        ] {
            assert!(
                ImageStore::open(&root, id, ImageStoreLimits::default()).is_err(),
                "{id:?}"
            );
        }
    }
}

use crate::{ImageResource, ViewResource};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::env;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const MAX_VIEW_BYTES: usize = 4 * 1024 * 1024;

/// An image file read from disk: where it is, what it is, and its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageFile {
    pub path: PathBuf,
    pub media_type: &'static str,
    /// The usual file extension for `media_type`.
    pub extension: &'static str,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

impl From<ImageFile> for ImageResource {
    fn from(file: ImageFile) -> Self {
        use base64::Engine as _;
        Self {
            path: file.path,
            media_type: file.media_type.into(),
            byte_len: file.bytes.len() as u64,
            data: base64::engine::general_purpose::STANDARD.encode(&file.bytes),
            sha256: file.sha256,
        }
    }
}

impl ImageResource {
    /// The file's bytes.
    pub fn bytes(&self) -> Result<Vec<u8>> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(self.data.as_bytes())
            .context("the image data is not valid base64")
    }
}

/// Read the image file `requested` names, resolved like a view resource:
/// relative to `cwd`, with `~` as the home directory. It must be a regular
/// file of at most [`crate::MAX_UPLOAD_IMAGE_BYTES`] whose header is a PNG,
/// JPEG, GIF or WebP.
pub fn load_image_file(requested: &Path, cwd: &Path) -> Result<ImageFile> {
    let limit = crate::MAX_UPLOAD_IMAGE_BYTES;
    let resolved = resolve_path(requested, cwd)?;
    let mut file = File::open(&resolved)
        .with_context(|| format!("cannot open image {}", resolved.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect image {}", resolved.display()))?;
    if !metadata.is_file() {
        bail!("image is not a regular file: {}", resolved.display());
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read image {}", resolved.display()))?;
    if bytes.len() as u64 > limit {
        bail!(
            "{} is larger than the {limit}-byte image limit",
            resolved.display()
        );
    }
    let Some((media_type, extension)) = xshell_execution::source_image_type(&bytes) else {
        bail!(
            "{} is not a supported image (PNG, JPEG, GIF or WebP)",
            resolved.display()
        );
    };
    Ok(ImageFile {
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        path: resolved,
        media_type,
        extension,
        bytes,
    })
}

pub fn load_view_resource(requested: &Path, cwd: &Path) -> Result<ViewResource> {
    let resolved = resolve_path(requested, cwd)?;
    let mut file = File::open(&resolved)
        .with_context(|| format!("cannot open view resource {}", resolved.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect view resource {}", resolved.display()))?;
    if !metadata.is_file() {
        bail!(
            "view resource is not a regular file: {}",
            resolved.display()
        );
    }
    if metadata.len() > MAX_VIEW_BYTES as u64 {
        bail!(
            "view resource exceeds the {} byte text-view limit",
            MAX_VIEW_BYTES
        );
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take((MAX_VIEW_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read view resource {}", resolved.display()))?;
    if bytes.len() > MAX_VIEW_BYTES {
        bail!(
            "view resource exceeds the {} byte text-view limit",
            MAX_VIEW_BYTES
        );
    }
    let byte_len = bytes.len() as u64;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let content = String::from_utf8(bytes).with_context(|| {
        format!(
            "view resource {} is not UTF-8 text; binary viewers are not implemented yet",
            resolved.display()
        )
    })?;
    Ok(ViewResource {
        media_type: media_type(&resolved).into(),
        path: resolved,
        content,
        byte_len,
        sha256,
    })
}

fn resolve_path(requested: &Path, cwd: &Path) -> Result<PathBuf> {
    if requested.as_os_str().is_empty() {
        bail!("view resource path is empty");
    }
    let expanded = if requested == Path::new("~") {
        home_dir()?
    } else if let Ok(rest) = requested.strip_prefix("~/") {
        home_dir()?.join(rest)
    } else if requested.is_absolute() {
        requested.to_owned()
    } else {
        cwd.join(requested)
    };
    expanded
        .canonicalize()
        .with_context(|| format!("cannot resolve view resource {}", expanded.display()))
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set on the session host")
}

fn media_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("md" | "markdown" | "mdown" | "mkd") => "text/markdown",
        Some("rst" | "rest") => "text/x-rst",
        _ => "text/plain",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn loads_bounded_utf8_and_reports_metadata() {
        let temporary = TempDir::new().unwrap();
        fs::write(temporary.path().join("guide.rst"), "Guide\n=====\n").unwrap();
        let resource = load_view_resource(Path::new("guide.rst"), temporary.path()).unwrap();
        assert_eq!(resource.content, "Guide\n=====\n");
        assert_eq!(resource.media_type, "text/x-rst");
        assert_eq!(resource.byte_len, 12);
        assert_eq!(resource.sha256.len(), 64);
        assert!(resource.path.is_absolute());
    }

    #[test]
    fn loads_an_image_file_by_its_header_not_its_name() {
        let temporary = TempDir::new().unwrap();
        // The smallest PNG header `image` recognises is its signature.
        let png = b"\x89PNG\r\n\x1a\nrest";
        fs::write(temporary.path().join("render.dat"), png).unwrap();
        fs::write(temporary.path().join("notes.png"), "plain text").unwrap();
        let image = load_image_file(Path::new("render.dat"), temporary.path()).unwrap();
        assert_eq!((image.media_type, image.extension), ("image/png", "png"));
        assert_eq!(image.bytes, png);
        assert!(image.path.is_absolute());
        let resource = ImageResource::from(image);
        assert_eq!(resource.byte_len, png.len() as u64);
        assert_eq!(resource.bytes().unwrap(), png);
        assert_eq!(resource.sha256.len(), 64);
        assert!(load_image_file(Path::new("notes.png"), temporary.path()).is_err());
        assert!(load_image_file(Path::new("missing.png"), temporary.path()).is_err());
        assert!(load_image_file(Path::new("."), temporary.path()).is_err());
    }

    #[test]
    fn rejects_binary_and_oversized_resources() {
        let temporary = TempDir::new().unwrap();
        fs::write(temporary.path().join("binary.md"), [0xff, 0xfe]).unwrap();
        assert!(load_view_resource(Path::new("binary.md"), temporary.path()).is_err());
        fs::write(
            temporary.path().join("huge.md"),
            vec![b'x'; MAX_VIEW_BYTES + 1],
        )
        .unwrap();
        assert!(load_view_resource(Path::new("huge.md"), temporary.path()).is_err());
    }
}

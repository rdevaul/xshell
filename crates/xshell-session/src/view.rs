use crate::ViewResource;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::env;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const MAX_VIEW_BYTES: usize = 4 * 1024 * 1024;

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

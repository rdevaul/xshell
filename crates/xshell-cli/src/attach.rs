//! Reading image files the user attaches to a message.
//!
//! The client only reads the file, bounded; the session host decodes,
//! checks and re-encodes it when it is uploaded.

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Read `path` for upload: its label (the file name) and its bytes, at most
/// the session host's upload limit.
pub fn read_attachment(path: &Path) -> Result<(String, Vec<u8>)> {
    let limit = xshell_session::MAX_UPLOAD_IMAGE_BYTES;
    let file =
        std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read {}", path.display()))?;
    if bytes.len() as u64 > limit {
        bail!(
            "{} is larger than the {limit}-byte image limit",
            path.display()
        );
    }
    let label = path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    Ok((label, bytes))
}

/// The file a `//attach --local` path names on this machine: `~` is the
/// home directory, and a relative path is taken from `base`.
pub fn resolve_local_path(path: &str, base: &Path) -> Result<PathBuf> {
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")
    };
    Ok(if path == "~" {
        home()?
    } else if let Some(rest) = path.strip_prefix("~/") {
        home()?.join(rest)
    } else {
        base.join(path)
    })
}

/// Where a `//attach` argument says the image is.
#[derive(Debug, PartialEq, Eq)]
pub enum AttachSource {
    /// A file on the session host, named as the session sees it.
    SessionHost(PathBuf),
    /// A file on the machine xshell runs on.
    Local(String),
}

/// Parse `[--local] PATH`. The path is one shell word, so a completed
/// `my\ render.png` or a quoted path works.
pub fn parse_attach_source(argument: &str) -> Result<AttachSource> {
    const USAGE: &str = "usage: //attach [--local] PATH (quote a path that contains spaces)";
    let words = shell_words::split(argument).context("invalid //attach quoting")?;
    match words.as_slice() {
        [path] if path != "--local" => Ok(AttachSource::SessionHost(path.into())),
        [flag, path] if flag == "--local" => Ok(AttachSource::Local(path.clone())),
        _ => bail!(USAGE),
    }
}

/// `//attach [[--local] PATH | --clear]`: store an image for the next agent
/// message, list what is pending, or drop it. Attachments need the session
/// fabric, because the session host stores and checks images; standalone
/// xshell refuses them.
///
/// PATH names a file on the session host, relative to the session's working
/// directory, as `//view` does. With `--local` it names a file on this
/// machine instead, relative to `local_base`, and the client uploads it. The
/// two differ only for a session on another host.
pub fn handle_attach(
    argument: &str,
    local_base: &Path,
    sessions: &mut crate::session::SessionRuntime,
    active: &crate::config::ActiveModel,
    pending: &mut Vec<xshell_core::ImageRef>,
) -> Result<String> {
    let describe = |image: &xshell_core::ImageRef| {
        format!("{} ({}x{})", image.label(), image.width(), image.height())
    };
    match argument.trim() {
        "" => {
            return Ok(if pending.is_empty() {
                "no images attached; usage: //attach [--local] PATH".into()
            } else {
                format!(
                    "attached for the next message: {}",
                    pending.iter().map(describe).collect::<Vec<_>>().join(", ")
                )
            });
        }
        "--clear" => {
            pending.clear();
            return Ok("attachments cleared".into());
        }
        _ => {}
    }
    if !sessions.enabled() {
        bail!(
            "//attach needs the session fabric (session_fabric.enabled = true), which stores \
and checks images"
        );
    }
    if !active.vision {
        bail!("this model is not configured for images; set `vision = true` on its model profile");
    }
    let image = match parse_attach_source(argument)? {
        AttachSource::SessionHost(path) => sessions
            .attach_image(path.clone())
            .with_context(|| format!("cannot attach {}", path.display()))?,
        AttachSource::Local(path) => {
            let path = resolve_local_path(&path, local_base)?;
            let (label, bytes) = read_attachment(&path)?;
            sessions
                .upload_image(&bytes, label)
                .with_context(|| format!("cannot attach {}", path.display()))?
        }
    };
    let message = format!(
        "attached {}; it goes with your next message",
        describe(&image)
    );
    pending.push(image);
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_file_with_its_name_as_label() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("render.png");
        std::fs::write(&path, b"bytes").unwrap();
        assert_eq!(
            read_attachment(&path).unwrap(),
            ("render.png".to_owned(), b"bytes".to_vec())
        );
        assert!(read_attachment(&directory.path().join("missing.png")).is_err());
    }

    #[test]
    fn resolves_local_paths_from_the_base_and_home() {
        let base = Path::new("/work/project");
        let resolve = |path| resolve_local_path(path, base).unwrap();
        assert_eq!(resolve("photos/a.jpg"), base.join("photos/a.jpg"));
        assert_eq!(resolve("/tmp/a.jpg"), Path::new("/tmp/a.jpg"));
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(resolve("~/a.jpg"), home.join("a.jpg"));
        assert_eq!(resolve("~"), home);
    }

    #[test]
    fn parses_where_the_image_is() {
        let host = |path: &str| AttachSource::SessionHost(path.into());
        let parse = |argument| parse_attach_source(argument).unwrap();
        assert_eq!(parse(" photos/a.jpg "), host("photos/a.jpg"));
        assert_eq!(parse("my\\ render.png"), host("my render.png"));
        assert_eq!(parse("'my render.png'"), host("my render.png"));
        assert_eq!(parse("~/a.jpg"), host("~/a.jpg"));
        assert_eq!(
            parse("--local shots/a.png"),
            AttachSource::Local("shots/a.png".into())
        );
        for refused in ["a.jpg b.jpg", "--local", "--local a b", "'unclosed"] {
            assert!(parse_attach_source(refused).is_err(), "{refused}");
        }
    }
}

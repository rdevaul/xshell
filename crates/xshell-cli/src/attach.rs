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

/// The file a `//attach` argument names. The argument is one shell word, so
/// a completed `my\ render.png` or a quoted path works. `~` is the home
/// directory, and a relative path is taken from `base`.
pub fn resolve_attachment_path(argument: &str, base: &Path) -> Result<PathBuf> {
    let mut words = shell_words::split(argument)
        .context("invalid //attach quoting")?
        .into_iter();
    let (Some(word), None) = (words.next(), words.next()) else {
        bail!("usage: //attach PATH (quote a path that contains spaces)");
    };
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")
    };
    Ok(if word == "~" {
        home()?
    } else if let Some(rest) = word.strip_prefix("~/") {
        home()?.join(rest)
    } else {
        base.join(word)
    })
}

/// `//attach [PATH | --clear]`: upload an image for the next agent message,
/// list what is pending, or drop it. Attachments need the session fabric,
/// because the session host stores and checks images; standalone xshell
/// refuses them. A relative PATH is taken from `base`.
pub fn handle_attach(
    argument: &str,
    base: &Path,
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
                "no images attached; usage: //attach PATH".into()
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
    let path = &resolve_attachment_path(argument, base)?;
    let (label, bytes) = read_attachment(path)?;
    let image = sessions
        .upload_image(&bytes, label)
        .with_context(|| format!("cannot attach {}", path.display()))?;
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
    fn resolves_relative_home_and_quoted_paths() {
        let base = Path::new("/work/project");
        let resolve = |argument| resolve_attachment_path(argument, base).unwrap();
        assert_eq!(resolve("photos/a.jpg"), base.join("photos/a.jpg"));
        assert_eq!(resolve(" /tmp/a.jpg "), Path::new("/tmp/a.jpg"));
        assert_eq!(resolve("my\\ render.png"), base.join("my render.png"));
        assert_eq!(resolve("'my render.png'"), base.join("my render.png"));
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(resolve("~/a.jpg"), home.join("a.jpg"));
        assert!(resolve_attachment_path("a.jpg b.jpg", base).is_err());
        assert!(resolve_attachment_path("'unclosed", base).is_err());
    }
}

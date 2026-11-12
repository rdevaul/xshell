//! Reading image files the user attaches to a message.
//!
//! The client only reads the file, bounded; the session host decodes,
//! checks and re-encodes it when it is uploaded.

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::Path;

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
        bail!("{} is larger than the {limit}-byte image limit", path.display());
    }
    let label = path
        .file_name()
        .map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned());
    Ok((label, bytes))
}

/// `//attach [PATH | --clear]`: upload an image for the next agent message,
/// list what is pending, or drop it. Attachments need the session fabric,
/// because the session host stores and checks images; standalone xshell
/// refuses them.
pub fn handle_attach(
    argument: &str,
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
    let path = Path::new(argument.trim());
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
}

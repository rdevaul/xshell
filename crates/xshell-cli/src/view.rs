//! `//view`: option parsing and rendering of local or session-host files.

use crate::audit::AuditRuntime;
use crate::session::SessionRuntime;
use anyhow::{Context, Result, bail};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use xshell_audit::AuditEvent;
use xshell_session::load_view_resource;
use xshell_view::{RenderOptions, ViewInput, ViewerRegistry};

pub(crate) struct ViewOptions {
    pub(crate) path: PathBuf,
    pub(crate) viewer: Option<String>,
}

pub(crate) fn parse_view_options(arguments: &str) -> Result<ViewOptions> {
    let words = shell_words::split(arguments).context("invalid //view quoting")?;
    let mut path = None;
    let mut viewer = None;
    let mut parse_options = true;
    let mut index = 0;
    while index < words.len() {
        let word = &words[index];
        if parse_options && word == "--" {
            parse_options = false;
        } else if parse_options && word == "--as" {
            index += 1;
            let value = words
                .get(index)
                .context("//view --as requires a viewer name")?;
            if viewer.replace(value.clone()).is_some() {
                bail!("//view accepts --as only once");
            }
        } else if parse_options && word.starts_with("--as=") {
            let value = word.trim_start_matches("--as=");
            if value.is_empty() {
                bail!("//view --as requires a viewer name");
            }
            if viewer.replace(value.into()).is_some() {
                bail!("//view accepts --as only once");
            }
        } else if parse_options && word.starts_with('-') {
            bail!("unknown //view option {word:?}");
        } else if path.replace(PathBuf::from(word)).is_some() {
            bail!("//view accepts exactly one path");
        }
        index += 1;
    }
    Ok(ViewOptions {
        path: path.context("usage: //view [--as VIEWER] PATH")?,
        viewer,
    })
}

pub(crate) fn handle_view(
    arguments: &str,
    sessions: &mut SessionRuntime,
    cwd: &Path,
    viewers: &ViewerRegistry,
    render_options: RenderOptions,
    audit: &mut AuditRuntime,
) -> Result<()> {
    let options = parse_view_options(arguments)?;
    let requested_path = options.path.display().to_string();
    let resource = match if sessions.enabled() {
        sessions.view_source(options.path.clone())
    } else {
        load_view_resource(&options.path, cwd)
    } {
        Ok(resource) => resource,
        Err(error) => {
            audit.append(AuditEvent::ViewOperation {
                path: requested_path,
                sha256: None,
                viewer: options.viewer,
                media_type: None,
                byte_len: None,
                outcome: format!("acquisition failed: {error:#}"),
            })?;
            return Err(error);
        }
    };

    let rendered = match viewers.render(
        &ViewInput {
            name: &resource.path.to_string_lossy(),
            media_type: &resource.media_type,
            text: &resource.content,
        },
        options.viewer.as_deref(),
        render_options,
    ) {
        Ok(rendered) => rendered,
        Err(error) => {
            audit.append(AuditEvent::ViewOperation {
                path: resource.path.display().to_string(),
                sha256: Some(resource.sha256),
                viewer: options.viewer,
                media_type: Some(resource.media_type),
                byte_len: Some(resource.byte_len),
                outcome: format!("render failed: {error:#}"),
            })?;
            return Err(error);
        }
    };

    let mut stdout = io::stdout();
    if let Err(error) = stdout
        .write_all(&rendered.bytes)
        .and_then(|()| stdout.flush())
    {
        audit.append(AuditEvent::ViewOperation {
            path: resource.path.display().to_string(),
            sha256: Some(resource.sha256),
            viewer: Some(rendered.viewer_id),
            media_type: Some(resource.media_type),
            byte_len: Some(resource.byte_len),
            outcome: format!("display failed: {error}"),
        })?;
        return Err(error).context("cannot display view resource");
    }
    audit.append(AuditEvent::ViewOperation {
        path: resource.path.display().to_string(),
        sha256: Some(resource.sha256),
        viewer: Some(rendered.viewer_id),
        media_type: Some(resource.media_type),
        byte_len: Some(resource.byte_len),
        outcome: "rendered".into(),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_view_path_and_explicit_viewer() {
        let options = parse_view_options("--as rst \"docs/design notes.rst\"").unwrap();
        assert_eq!(options.path, Path::new("docs/design notes.rst"));
        assert_eq!(options.viewer.as_deref(), Some("rst"));

        let options = parse_view_options("--as=markdown -- -draft.md").unwrap();
        assert_eq!(options.path, Path::new("-draft.md"));
        assert_eq!(options.viewer.as_deref(), Some("markdown"));
        assert!(parse_view_options("").is_err());
        assert!(parse_view_options("one.md two.md").is_err());
    }
}

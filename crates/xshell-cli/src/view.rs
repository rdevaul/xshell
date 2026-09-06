//! `//view`: option parsing and rendering of local or session-host files.

use crate::audit::AuditRuntime;
use crate::session::SessionRuntime;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use xshell_audit::AuditEvent;
use xshell_session::load_view_resource;
use xshell_view::{RenderOptions, ViewInput, ViewerRegistry};

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PaginationMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ViewPolicy {
    pub(crate) pagination: Option<PaginationMode>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ViewConfig {
    pub(crate) pagination: PaginationMode,
    pub(crate) pager: Vec<String>,
    pub(crate) classes: BTreeMap<String, ViewPolicy>,
    pub(crate) viewers: BTreeMap<String, ViewPolicy>,
}

impl Default for ViewConfig {
    fn default() -> Self {
        Self {
            pagination: PaginationMode::Auto,
            pager: vec!["less".into(), "-R".into(), "-X".into()],
            classes: BTreeMap::new(),
            viewers: BTreeMap::new(),
        }
    }
}

impl ViewConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.pager.first().is_none_or(String::is_empty) {
            bail!("view.pager must contain a non-empty executable name");
        }
        if self.classes.keys().any(String::is_empty) {
            bail!("view.classes keys cannot be empty");
        }
        if self.viewers.keys().any(String::is_empty) {
            bail!("view.viewers keys cannot be empty");
        }
        Ok(())
    }

    fn pagination_for(
        &self,
        viewer_id: &str,
        media_class: &str,
        command_override: Option<PaginationMode>,
    ) -> PaginationMode {
        command_override
            .or_else(|| {
                self.viewers
                    .get(viewer_id)
                    .and_then(|policy| policy.pagination)
            })
            .or_else(|| {
                self.classes
                    .get(media_class)
                    .and_then(|policy| policy.pagination)
            })
            .unwrap_or(self.pagination)
    }
}

pub(crate) struct ViewOptions {
    pub(crate) path: PathBuf,
    pub(crate) viewer: Option<String>,
    pub(crate) pagination: Option<PaginationMode>,
}

pub(crate) fn parse_view_options(arguments: &str) -> Result<ViewOptions> {
    let words = shell_words::split(arguments).context("invalid //view quoting")?;
    let mut path = None;
    let mut viewer = None;
    let mut pagination = None;
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
        } else if parse_options && word == "--paginate" {
            if pagination.replace(PaginationMode::Always).is_some() {
                bail!("//view accepts only one pagination override");
            }
        } else if parse_options && word == "--no-paginate" {
            if pagination.replace(PaginationMode::Never).is_some() {
                bail!("//view accepts only one pagination override");
            }
        } else if parse_options && word.starts_with('-') {
            bail!("unknown //view option {word:?}");
        } else if path.replace(PathBuf::from(word)).is_some() {
            bail!("//view accepts exactly one path");
        }
        index += 1;
    }
    Ok(ViewOptions {
        path: path.context("usage: //view [--as VIEWER] [--paginate|--no-paginate] PATH")?,
        viewer,
        pagination,
    })
}

pub(crate) fn handle_view(
    arguments: &str,
    sessions: &mut SessionRuntime,
    cwd: &Path,
    viewers: &ViewerRegistry,
    render_options: RenderOptions,
    view_config: &ViewConfig,
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

    let pagination = view_config.pagination_for(
        &rendered.viewer_id,
        &rendered.media_class,
        options.pagination,
    );
    let display = match display_view(&rendered.bytes, pagination, view_config) {
        Ok(display) => display,
        Err(error) => {
            audit.append(AuditEvent::ViewOperation {
                path: resource.path.display().to_string(),
                sha256: Some(resource.sha256),
                viewer: Some(rendered.viewer_id.clone()),
                media_type: Some(resource.media_type),
                byte_len: Some(resource.byte_len),
                outcome: format!("display failed: {error}"),
            })?;
            return Err(error);
        }
    };
    audit.append(AuditEvent::ViewOperation {
        path: resource.path.display().to_string(),
        sha256: Some(resource.sha256),
        viewer: Some(rendered.viewer_id),
        media_type: Some(resource.media_type),
        byte_len: Some(resource.byte_len),
        outcome: format!("rendered ({})", display.as_str()),
    })?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayMethod {
    Direct,
    Pager,
}

enum PagerOutcome {
    Displayed,
    Unavailable(anyhow::Error),
}

impl DisplayMethod {
    fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Pager => "pager",
        }
    }
}

fn display_view(
    bytes: &[u8],
    pagination: PaginationMode,
    config: &ViewConfig,
) -> Result<DisplayMethod> {
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let rows = detected_terminal_rows();
    let mut stdout = io::stdout();
    display_view_with(
        bytes,
        pagination,
        interactive,
        rows,
        &config.pager,
        &mut stdout,
        run_pager,
    )
}

fn display_view_with(
    bytes: &[u8],
    pagination: PaginationMode,
    interactive: bool,
    terminal_rows: usize,
    pager: &[String],
    direct_output: &mut impl Write,
    mut page: impl FnMut(&[String], &[u8]) -> Result<PagerOutcome>,
) -> Result<DisplayMethod> {
    let usable_rows = terminal_rows.saturating_sub(1).max(1);
    let should_page = interactive
        && match pagination {
            PaginationMode::Auto => rendered_rows(bytes) > usable_rows,
            PaginationMode::Always => true,
            PaginationMode::Never => false,
        };
    if should_page {
        match page(pager, bytes) {
            Ok(PagerOutcome::Displayed) => return Ok(DisplayMethod::Pager),
            Ok(PagerOutcome::Unavailable(error)) if pagination == PaginationMode::Auto => {
                eprintln!("xshell: pager unavailable; displaying inline: {error:#}");
            }
            Ok(PagerOutcome::Unavailable(error)) => return Err(error),
            Err(error) => return Err(error),
        }
    }
    direct_output
        .write_all(bytes)
        .and_then(|()| direct_output.flush())
        .context("cannot display view resource")?;
    Ok(DisplayMethod::Direct)
}

fn run_pager(pager: &[String], bytes: &[u8]) -> Result<PagerOutcome> {
    let (program, arguments) = pager
        .split_first()
        .context("view pager command must contain an executable")?;
    if program.is_empty() {
        bail!("view pager executable cannot be empty");
    }
    let child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        // Do not let less invoke preprocessors selected by inherited state.
        // Secure mode also disables less's shell and file-opening commands.
        .env_remove("LESS")
        .env_remove("LESSKEY")
        .env_remove("LESSKEYIN")
        .env_remove("LESSOPEN")
        .env_remove("LESSCLOSE")
        .env("LESSSECURE", "1")
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            return Ok(PagerOutcome::Unavailable(
                anyhow::Error::new(error).context(format!("cannot start view pager {program:?}")),
            ));
        }
    };
    let write_result = child
        .stdin
        .take()
        .context("view pager stdin is unavailable")?
        .write_all(bytes);
    let status = child.wait().context("cannot wait for view pager")?;
    if !status.success() {
        bail!("view pager exited with {status}");
    }
    if let Err(error) = write_result
        && error.kind() != io::ErrorKind::BrokenPipe
    {
        return Err(error).context("cannot write rendered content to view pager");
    }
    Ok(PagerOutcome::Displayed)
}

fn rendered_rows(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    bytes.iter().filter(|byte| **byte == b'\n').count() + usize::from(bytes.last() != Some(&b'\n'))
}

fn detected_terminal_rows() -> usize {
    std::env::var("LINES")
        .ok()
        .and_then(|value| value.parse().ok())
        .or_else(|| xshell_pty::controller_size().map(|size| usize::from(size.rows)))
        .unwrap_or(24)
        .max(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_view_path_and_explicit_viewer() {
        let options = parse_view_options("--as rst \"docs/design notes.rst\"").unwrap();
        assert_eq!(options.path, Path::new("docs/design notes.rst"));
        assert_eq!(options.viewer.as_deref(), Some("rst"));
        assert_eq!(options.pagination, None);

        let options = parse_view_options("--paginate --as=markdown -- -draft.md").unwrap();
        assert_eq!(options.path, Path::new("-draft.md"));
        assert_eq!(options.viewer.as_deref(), Some("markdown"));
        assert_eq!(options.pagination, Some(PaginationMode::Always));

        let options = parse_view_options("guide.rst --no-paginate").unwrap();
        assert_eq!(options.pagination, Some(PaginationMode::Never));
        assert!(parse_view_options("").is_err());
        assert!(parse_view_options("one.md two.md").is_err());
        assert!(parse_view_options("--paginate --no-paginate one.md").is_err());
    }

    #[test]
    fn pagination_policy_uses_command_viewer_class_default_precedence() {
        let config: ViewConfig = toml::from_str(
            r#"
            pagination = "never"

            [classes.text]
            pagination = "auto"

            [viewers.markdown]
            pagination = "always"
            "#,
        )
        .unwrap();

        assert_eq!(
            config.pagination_for("markdown", "text", None),
            PaginationMode::Always
        );
        assert_eq!(
            config.pagination_for("rst", "text", None),
            PaginationMode::Auto
        );
        assert_eq!(
            config.pagination_for("image", "image", None),
            PaginationMode::Never
        );
        assert_eq!(
            config.pagination_for("markdown", "text", Some(PaginationMode::Never)),
            PaginationMode::Never
        );
    }

    #[test]
    fn automatic_pagination_uses_terminal_height_and_configured_argv() {
        let bytes = b"one\ntwo\nthree\n";
        let pager = vec!["test-pager".into(), "--safe".into()];
        let mut direct = Vec::new();
        let mut paged = Vec::new();
        let method = display_view_with(
            bytes,
            PaginationMode::Auto,
            true,
            3,
            &pager,
            &mut direct,
            |observed_pager, observed_bytes| {
                assert_eq!(observed_pager, pager);
                paged.extend_from_slice(observed_bytes);
                Ok(PagerOutcome::Displayed)
            },
        )
        .unwrap();
        assert_eq!(method, DisplayMethod::Pager);
        assert_eq!(paged, bytes);
        assert!(direct.is_empty());

        let mut direct = Vec::new();
        let method = display_view_with(
            b"one\ntwo\n",
            PaginationMode::Auto,
            true,
            4,
            &pager,
            &mut direct,
            |_, _| panic!("short output must not start a pager"),
        )
        .unwrap();
        assert_eq!(method, DisplayMethod::Direct);
        assert_eq!(direct, b"one\ntwo\n");
    }

    #[test]
    fn paging_is_disabled_when_noninteractive_and_auto_falls_back_inline() {
        let pager = vec!["missing-pager".into()];
        let mut direct = Vec::new();
        let method = display_view_with(
            b"one\ntwo\nthree\n",
            PaginationMode::Always,
            false,
            2,
            &pager,
            &mut direct,
            |_, _| panic!("noninteractive output must not start a pager"),
        )
        .unwrap();
        assert_eq!(method, DisplayMethod::Direct);
        assert_eq!(direct, b"one\ntwo\nthree\n");

        direct.clear();
        let method = display_view_with(
            b"one\ntwo\nthree\n",
            PaginationMode::Auto,
            true,
            2,
            &pager,
            &mut direct,
            |_, _| Ok(PagerOutcome::Unavailable(anyhow::anyhow!("not installed"))),
        )
        .unwrap();
        assert_eq!(method, DisplayMethod::Direct);
        assert_eq!(direct, b"one\ntwo\nthree\n");

        let error = display_view_with(
            b"long\n",
            PaginationMode::Always,
            true,
            24,
            &pager,
            &mut Vec::new(),
            |_, _| Ok(PagerOutcome::Unavailable(anyhow::anyhow!("not installed"))),
        )
        .unwrap_err();
        assert!(error.to_string().contains("not installed"));

        let error = display_view_with(
            b"long\nagain\n",
            PaginationMode::Auto,
            true,
            1,
            &pager,
            &mut Vec::new(),
            |_, _| bail!("pager failed after starting"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("failed after starting"));
    }

    #[test]
    fn missing_pager_is_classified_before_any_output_is_displayed() {
        let pager = vec!["xshell-test-pager-that-does-not-exist".into()];
        let outcome = run_pager(&pager, b"rendered").unwrap();
        match outcome {
            PagerOutcome::Unavailable(error) => {
                assert!(error.to_string().contains("cannot start view pager"));
            }
            PagerOutcome::Displayed => panic!("missing pager was reported as displayed"),
        }
        assert!(run_pager(&[], b"rendered").is_err());
    }
}

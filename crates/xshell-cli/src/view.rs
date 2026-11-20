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
use xshell_session::{load_image_file, load_view_resource};
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
    /// The command `//view` runs on an image file, with the file's path
    /// appended.
    pub(crate) image_viewer: Vec<String>,
    pub(crate) classes: BTreeMap<String, ViewPolicy>,
    pub(crate) viewers: BTreeMap<String, ViewPolicy>,
}

/// The desktop's own "open this file" command.
const DEFAULT_IMAGE_OPENER: &str = if cfg!(target_os = "macos") {
    "open"
} else {
    "xdg-open"
};

impl Default for ViewConfig {
    fn default() -> Self {
        Self {
            pagination: PaginationMode::Auto,
            pager: vec!["less".into(), "-R".into(), "-X".into()],
            image_viewer: vec![DEFAULT_IMAGE_OPENER.into()],
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
        if self.image_viewer.first().is_none_or(String::is_empty) {
            bail!("view.image_viewer must contain a non-empty executable name");
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

/// Image files copied from another host for a local viewer. They are kept in
/// a private directory that is removed when xshell exits.
#[derive(Default)]
pub(crate) struct ViewedImages {
    directory: Option<tempfile::TempDir>,
    stored: usize,
}

impl ViewedImages {
    /// Write `bytes` to a new file named after `source`, which another host
    /// supplied: only its plain characters are kept.
    fn store(&mut self, source: &Path, extension: &str, bytes: &[u8]) -> Result<PathBuf> {
        let directory = match &mut self.directory {
            Some(directory) => directory,
            empty => empty.insert(
                tempfile::Builder::new()
                    .prefix("xshell-view-")
                    .tempdir()
                    .context("cannot create a directory for viewed images")?,
            ),
        };
        let stem: String = source
            .file_stem()
            .map(|stem| stem.to_string_lossy())
            .unwrap_or_default()
            .chars()
            .filter(|character| character.is_alphanumeric() || "-_ ".contains(*character))
            .take(64)
            .collect();
        self.stored += 1;
        let path = directory
            .path()
            .join(format!("{}-{stem}.{extension}", self.stored));
        std::fs::write(&path, bytes).with_context(|| format!("cannot write {}", path.display()))?;
        Ok(path)
    }
}

/// Whether `path` is named like an image `//view` hands to the image viewer.
fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["png", "jpg", "jpeg", "gif", "webp"]
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        })
}

/// An image ready for the viewer.
struct AcquiredImage {
    /// The file on this machine to open.
    file: PathBuf,
    /// The file's path on the session host.
    source: PathBuf,
    media_type: String,
    byte_len: u64,
    sha256: String,
}

/// Find the image `path` names. A file on this machine is opened where it
/// is; one on another host is copied here first.
fn acquire_image(
    path: &Path,
    sessions: &mut SessionRuntime,
    cwd: &Path,
    viewed: &mut ViewedImages,
) -> Result<AcquiredImage> {
    if !sessions.is_remote() {
        let image = load_image_file(path, cwd)?;
        return Ok(AcquiredImage {
            file: image.path.clone(),
            source: image.path,
            media_type: image.media_type.into(),
            byte_len: image.bytes.len() as u64,
            sha256: image.sha256,
        });
    }
    let resource = sessions.view_image(path.to_owned())?;
    let bytes = resource.bytes()?;
    // The other host says what it sent; check before a viewer is given it.
    let Some((media_type, extension)) = xshell_execution::source_image_type(&bytes) else {
        bail!("the session host did not send a supported image (PNG, JPEG, GIF or WebP)");
    };
    Ok(AcquiredImage {
        file: viewed.store(&resource.path, extension, &bytes)?,
        source: resource.path,
        media_type: media_type.into(),
        byte_len: bytes.len() as u64,
        sha256: resource.sha256,
    })
}

/// Start `viewer` on `file` and return without waiting for it: a desktop
/// viewer stays open while the user goes on working.
fn open_image_viewer(viewer: &[String], file: &Path) -> Result<()> {
    let (program, arguments) = viewer
        .split_first()
        .context("view.image_viewer must contain an executable")?;
    let mut child = Command::new(program)
        .args(arguments)
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("cannot start image viewer {program:?}"))?;
    // Reap the viewer whenever it exits.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// `//view` on an image: open it in the local image viewer.
fn view_image(
    path: &Path,
    sessions: &mut SessionRuntime,
    cwd: &Path,
    view_config: &ViewConfig,
    audit: &mut AuditRuntime,
    viewed: &mut ViewedImages,
) -> Result<()> {
    let requested_path = path.display().to_string();
    let image = match acquire_image(path, sessions, cwd, viewed) {
        Ok(image) => image,
        Err(error) => {
            audit.append(AuditEvent::ViewOperation {
                path: requested_path,
                sha256: None,
                viewer: Some("image".into()),
                media_type: None,
                byte_len: None,
                outcome: format!("acquisition failed: {error:#}"),
            })?;
            return Err(error);
        }
    };
    let opened = open_image_viewer(&view_config.image_viewer, &image.file);
    audit.append(AuditEvent::ViewOperation {
        path: image.source.display().to_string(),
        sha256: Some(image.sha256),
        viewer: Some("image".into()),
        media_type: Some(image.media_type),
        byte_len: Some(image.byte_len),
        outcome: match &opened {
            Ok(()) => format!("opened with {}", view_config.image_viewer[0]),
            Err(error) => format!("display failed: {error:#}"),
        },
    })?;
    opened?;
    println!(
        "opened {requested_path} with {}",
        view_config.image_viewer[0]
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_view(
    arguments: &str,
    sessions: &mut SessionRuntime,
    cwd: &Path,
    viewers: &ViewerRegistry,
    render_options: RenderOptions,
    view_config: &ViewConfig,
    audit: &mut AuditRuntime,
    viewed_images: &mut ViewedImages,
) -> Result<()> {
    let options = parse_view_options(arguments)?;
    // An image goes to the image viewer unless a text viewer was asked for.
    if options.viewer.is_none() && is_image_path(&options.path) {
        return view_image(
            &options.path,
            sessions,
            cwd,
            view_config,
            audit,
            viewed_images,
        );
    }
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
    fn images_are_recognised_by_extension() {
        for image in ["a.png", "photos/B.JPG", "c.jpeg", "d.gif", "e.webp"] {
            assert!(is_image_path(Path::new(image)), "{image}");
        }
        for other in ["notes.md", "png", "archive.png.txt", "render.svg"] {
            assert!(!is_image_path(Path::new(other)), "{other}");
        }
    }

    #[test]
    fn image_viewer_is_configurable_and_must_name_a_program() {
        assert_eq!(
            ViewConfig::default().image_viewer,
            [DEFAULT_IMAGE_OPENER.to_owned()]
        );
        let config: ViewConfig =
            toml::from_str(r#"image_viewer = ["feh", "--scale-down"]"#).unwrap();
        assert_eq!(config.image_viewer, ["feh", "--scale-down"]);
        config.validate().unwrap();
        let empty: ViewConfig = toml::from_str("image_viewer = []").unwrap();
        assert!(empty.validate().is_err());
    }

    #[test]
    fn viewed_images_get_plain_names_in_a_directory_removed_on_drop() {
        let mut viewed = ViewedImages::default();
        let first = viewed
            .store(Path::new("/remote/../x/my render;$(rm).png"), "png", b"one")
            .unwrap();
        let second = viewed.store(Path::new("/"), "jpg", b"two").unwrap();
        assert_eq!(first.file_name().unwrap(), "1-my renderrm.png");
        assert_eq!(second.file_name().unwrap(), "2-.jpg");
        assert_eq!(first.parent(), second.parent());
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        let directory = first.parent().unwrap().to_owned();
        drop(viewed);
        assert!(!directory.exists());
    }

    #[test]
    fn the_image_viewer_is_started_with_the_file_and_not_waited_for() {
        let directory = tempfile::TempDir::new().unwrap();
        let marker = directory.path().join("seen");
        // `sh -c 'cp "$1" "$0"' MARKER FILE` copies the viewed file to MARKER.
        let viewer = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            r#"cp "$1" "$0""#.to_owned(),
            marker.display().to_string(),
        ];
        let file = directory.path().join("a.png");
        std::fs::write(&file, b"image bytes").unwrap();
        open_image_viewer(&viewer, &file).unwrap();
        for _ in 0..200 {
            if std::fs::read(&marker).is_ok_and(|bytes| bytes == b"image bytes") {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the viewer was not run on the file");
    }

    #[test]
    fn a_missing_image_viewer_is_an_error() {
        let viewer = vec!["xshell-no-such-image-viewer".to_owned()];
        let error = open_image_viewer(&viewer, Path::new("/tmp/a.png")).unwrap_err();
        assert!(format!("{error:#}").contains("cannot start image viewer"));
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

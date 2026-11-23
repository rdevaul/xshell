//! Shell containment: where a session's shell commands may work.
//!
//! A session asks for a [`ShellContainment`] when it is created. The daemon
//! checks the request against its operator's [`ShellBounds`], resolves every
//! path once, and stores the result. What a session gets is the most
//! restrictive combination of what it asked for and what the operator
//! allows: a request outside the bounds refuses the session; it is never
//! narrowed silently.
//!
//! This module decides and records containment. Enforcing it is the job of
//! a `ShellRunner`. See `docs/shell-sandbox.md`.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};

/// Most roots, and most read-only grants, one session may name.
const MAX_PATHS: usize = 32;
/// Most secrets one session may ask for.
const MAX_SECRETS: usize = 16;

/// Where a session's shell commands may work.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShellContainment {
    /// With the full authority of the daemon's account.
    #[default]
    Ambient,
    /// Inside declared directories, enforced by the platform.
    Isolated(IsolatedShell),
}

impl ShellContainment {
    pub fn is_ambient(&self) -> bool {
        matches!(self, Self::Ambient)
    }
}

/// What an isolated session's commands may reach. Everything but `roots`
/// defaults to the most restrictive value.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct IsolatedShell {
    /// Directories commands may read and write. At least one.
    pub roots: Vec<PathBuf>,
    /// Further directories and files commands may read but not write.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub read_only: Vec<PathBuf>,
    /// Whether commands may use the network.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub network: bool,
    /// Names of secrets the daemon puts in commands' environment.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    /// Whether the commands a person types are contained too.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub user_commands: bool,
}

/// The least containment a daemon accepts.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MinimumContainment {
    #[default]
    Ambient,
    Isolated,
}

/// Where a named secret's value comes from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecretSource {
    /// The name of a variable in the daemon's own environment.
    pub env: String,
}

/// What the daemon's operator allows sessions to ask for
/// (`[session_fabric.shell]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ShellBounds {
    /// Refuse a session whose shell is less contained than this.
    pub minimum: MinimumContainment,
    /// When set, every root a session asks for must be inside one of these.
    /// When absent, a session may name any root: a host that allows ambient
    /// shells already lets commands reach everything.
    pub allowed_roots: Option<Vec<PathBuf>>,
    /// Every read-only grant a session asks for must be inside one of
    /// these. Absent or empty allows none.
    pub allowed_read_only: Vec<PathBuf>,
    /// Whether a session may turn the network on.
    pub allow_network: bool,
    /// The secrets a session may ask for by name.
    pub secrets: BTreeMap<String, SecretSource>,
}

impl ShellBounds {
    /// Check the bounds themselves, when the daemon starts.
    pub fn validate(&self) -> Result<()> {
        if self.minimum == MinimumContainment::Isolated && self.allowed_roots.is_none() {
            bail!(
                "session_fabric.shell.minimum = \"isolated\" needs \
session_fabric.shell.allowed_roots, so that sessions can only name roots the operator chose"
            );
        }
        for (name, source) in &self.secrets {
            validate_variable_name("secret", name)?;
            validate_variable_name("secret source variable", &source.env)?;
        }
        Ok(())
    }
}

/// Facts about the host that a request is checked against.
#[derive(Debug, Clone)]
pub struct Host<'a> {
    /// The daemon account's home directory, for `~`.
    pub home: Option<&'a Path>,
    /// Paths no root or grant may be, contain, or lie inside: the daemon's
    /// state directory, configuration file, audit directory and control
    /// token.
    pub protected: &'a [PathBuf],
}

/// Check `request` against `bounds` and resolve its paths.
///
/// Returns what to store for the session: the same request with every path
/// canonical. `cwd` is the session's working directory, already canonical.
pub fn resolve(
    request: &ShellContainment,
    bounds: &ShellBounds,
    cwd: &Path,
    host: &Host,
) -> Result<ShellContainment> {
    let isolated = match request {
        ShellContainment::Ambient => {
            if bounds.minimum == MinimumContainment::Isolated {
                bail!(
                    "this session host requires an isolated shell; ask for one with the \
directories the session may work in"
                );
            }
            return Ok(ShellContainment::Ambient);
        }
        ShellContainment::Isolated(isolated) => isolated,
    };

    if isolated.roots.is_empty() {
        bail!("an isolated shell needs at least one root directory");
    }
    if isolated.roots.len() > MAX_PATHS || isolated.read_only.len() > MAX_PATHS {
        bail!(
            "an isolated shell may name at most {MAX_PATHS} roots and {MAX_PATHS} read-only grants"
        );
    }

    let protected = canonical_protected(host.protected);
    let allowed_roots = bounds
        .allowed_roots
        .as_ref()
        .map(|allowed| canonical_bounds(allowed, host.home));
    let mut roots = Vec::new();
    for root in &isolated.roots {
        let resolved = resolve_path("root", root, host.home, true)?;
        refuse_protected("root", &resolved, &protected)?;
        if let Some(allowed) = &allowed_roots
            && !allowed.iter().any(|allowed| resolved.starts_with(allowed))
        {
            bail!(
                "root {} is outside the directories this session host allows \
(session_fabric.shell.allowed_roots)",
                resolved.display()
            );
        }
        if !roots.contains(&resolved) {
            roots.push(resolved);
        }
    }
    if !roots.iter().any(|root| cwd.starts_with(root)) {
        bail!(
            "the session's working directory {} is not inside any of its roots",
            cwd.display()
        );
    }

    let allowed_read_only = canonical_bounds(&bounds.allowed_read_only, host.home);
    let mut read_only = Vec::new();
    for grant in &isolated.read_only {
        let resolved = resolve_path("read-only grant", grant, host.home, false)?;
        refuse_protected("read-only grant", &resolved, &protected)?;
        if !allowed_read_only
            .iter()
            .any(|allowed| resolved.starts_with(allowed))
        {
            bail!(
                "read-only grant {} is outside what this session host allows \
(session_fabric.shell.allowed_read_only)",
                resolved.display()
            );
        }
        if !read_only.contains(&resolved) {
            read_only.push(resolved);
        }
    }

    if isolated.network && !bounds.allow_network {
        bail!(
            "this session host does not allow isolated shells to use the network \
(session_fabric.shell.allow_network)"
        );
    }

    if isolated.secrets.len() > MAX_SECRETS {
        bail!("an isolated shell may ask for at most {MAX_SECRETS} secrets");
    }
    let mut seen = HashSet::new();
    for name in &isolated.secrets {
        if !bounds.secrets.contains_key(name) {
            bail!(
                "this session host has no secret named {name:?} \
(session_fabric.shell.secrets)"
            );
        }
        if !seen.insert(name.as_str()) {
            bail!("secret {name:?} is listed twice");
        }
    }

    Ok(ShellContainment::Isolated(IsolatedShell {
        roots,
        read_only,
        network: isolated.network,
        secrets: isolated.secrets.clone(),
        user_commands: isolated.user_commands,
    }))
}

/// Expand `~`, require an absolute path, and resolve every link.
fn resolve_path(kind: &str, path: &Path, home: Option<&Path>, directory: bool) -> Result<PathBuf> {
    let expanded = expand_home(path, home)
        .with_context(|| format!("{kind} {} cannot be expanded", path.display()))?;
    if !expanded.is_absolute() {
        bail!("{kind} {} must be an absolute path", path.display());
    }
    let resolved = expanded
        .canonicalize()
        .with_context(|| format!("{kind} {} does not exist or cannot be read", path.display()))?;
    if resolved.parent().is_none() {
        bail!("{kind} {} is the whole filesystem", path.display());
    }
    if directory && !resolved.is_dir() {
        bail!("{kind} {} is not a directory", resolved.display());
    }
    Ok(resolved)
}

fn expand_home(path: &Path, home: Option<&Path>) -> Result<PathBuf> {
    let mut components = path.components();
    match components.next() {
        Some(Component::Normal(first)) if first == "~" => {
            let home = home.context("the session host has no home directory to expand ~ with")?;
            Ok(home.join(components.as_path()))
        }
        _ => Ok(path.to_owned()),
    }
}

/// The operator's bound paths, canonical where they exist. One that does
/// not exist cannot contain anything, so it is dropped.
fn canonical_bounds(bounds: &[PathBuf], home: Option<&Path>) -> Vec<PathBuf> {
    bounds
        .iter()
        .filter_map(|bound| expand_home(bound, home).ok())
        .filter_map(|bound| bound.canonicalize().ok())
        .collect()
}

/// The protected paths, canonical where they exist and as written where
/// they do not (a state directory may not have been created yet).
fn canonical_protected(protected: &[PathBuf]) -> Vec<PathBuf> {
    protected
        .iter()
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()))
        .collect()
}

/// A grant may not be a protected path, hold one, or lie inside one.
fn refuse_protected(kind: &str, path: &Path, protected: &[PathBuf]) -> Result<()> {
    for guarded in protected {
        if path.starts_with(guarded) || guarded.starts_with(path) {
            bail!(
                "{kind} {} would expose the session host's own files ({})",
                path.display(),
                guarded.display()
            );
        }
    }
    Ok(())
}

/// An environment variable name: letters, digits and `_`, not starting with
/// a digit.
fn validate_variable_name(kind: &str, name: &str) -> Result<()> {
    let mut chars = name.chars();
    let valid = name.len() <= 64
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        bail!("{kind} name {name:?} must be letters, digits and _, not starting with a digit");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A host with a home directory holding `src/app`, `src/lib`, `tools`,
    /// `private` and `state`.
    struct Fixture {
        _temporary: TempDir,
        home: PathBuf,
        protected: Vec<PathBuf>,
    }

    impl Fixture {
        fn new() -> Self {
            let temporary = TempDir::new().unwrap();
            let home = temporary.path().canonicalize().unwrap();
            for directory in ["src/app", "src/lib", "tools/bin", "private", "state"] {
                std::fs::create_dir_all(home.join(directory)).unwrap();
            }
            std::fs::write(home.join("tools/bin/cc"), "").unwrap();
            let protected = vec![home.join("state"), home.join("config.toml")];
            Self {
                _temporary: temporary,
                home,
                protected,
            }
        }

        fn host(&self) -> Host<'_> {
            Host {
                home: Some(&self.home),
                protected: &self.protected,
            }
        }

        fn resolve(
            &self,
            request: IsolatedShell,
            bounds: &ShellBounds,
        ) -> Result<ShellContainment> {
            resolve(
                &ShellContainment::Isolated(request),
                bounds,
                &self.home.join("src/app"),
                &self.host(),
            )
        }
    }

    fn roots(roots: &[&str]) -> IsolatedShell {
        IsolatedShell {
            roots: roots.iter().map(PathBuf::from).collect(),
            ..IsolatedShell::default()
        }
    }

    fn message(result: Result<ShellContainment>) -> String {
        format!("{:#}", result.unwrap_err())
    }

    #[test]
    fn ambient_is_the_default_and_stays_ambient_unless_the_host_requires_more() {
        let fixture = Fixture::new();
        assert!(ShellContainment::default().is_ambient());
        let ambient = |bounds: &ShellBounds| {
            resolve(
                &ShellContainment::Ambient,
                bounds,
                &fixture.home,
                &fixture.host(),
            )
        };
        assert_eq!(
            ambient(&ShellBounds::default()).unwrap(),
            ShellContainment::Ambient
        );
        let strict = ShellBounds {
            minimum: MinimumContainment::Isolated,
            allowed_roots: Some(vec!["~/src".into()]),
            ..ShellBounds::default()
        };
        assert!(message(ambient(&strict)).contains("requires an isolated shell"));
    }

    #[test]
    fn roots_are_expanded_resolved_and_deduplicated() {
        let fixture = Fixture::new();
        std::os::unix::fs::symlink(fixture.home.join("src/app"), fixture.home.join("link"))
            .unwrap();
        let resolved = fixture
            .resolve(
                roots(&["~/src/app", "~/link", "~/src/app/../lib"]),
                &ShellBounds::default(),
            )
            .unwrap();
        let ShellContainment::Isolated(isolated) = resolved else {
            panic!("expected an isolated shell");
        };
        // The link and the plain spelling are one root; `..` is gone.
        assert_eq!(
            isolated.roots,
            [fixture.home.join("src/app"), fixture.home.join("src/lib")]
        );
        // The restrictive defaults survive.
        assert!(!isolated.network && !isolated.user_commands);
        assert!(isolated.read_only.is_empty() && isolated.secrets.is_empty());
    }

    #[test]
    fn roots_that_are_missing_relative_too_wide_or_protected_are_refused() {
        let fixture = Fixture::new();
        let bounds = ShellBounds::default();
        for (root, expected) in [
            ("~/missing", "does not exist"),
            ("src/app", "must be an absolute path"),
            ("/", "is the whole filesystem"),
            ("~/tools/bin/cc", "is not a directory"),
            // The state directory itself, a directory inside it, and a
            // directory that holds it.
            ("~/state", "would expose the session host's own files"),
            ("~", "would expose the session host's own files"),
        ] {
            let error = message(fixture.resolve(roots(&["~/src/app", root]), &bounds));
            assert!(error.contains(expected), "{root}: {error}");
        }
        assert!(message(fixture.resolve(roots(&[]), &bounds)).contains("at least one root"));
        // The working directory (src/app) must be inside a root.
        let error = message(fixture.resolve(roots(&["~/src/lib"]), &bounds));
        assert!(error.contains("working directory"), "{error}");
    }

    #[test]
    fn a_root_must_be_inside_the_operators_allowed_roots_after_resolving_links() {
        let fixture = Fixture::new();
        let bounds = ShellBounds {
            allowed_roots: Some(vec!["~/src".into()]),
            ..ShellBounds::default()
        };
        fixture.resolve(roots(&["~/src/app"]), &bounds).unwrap();
        // A link inside the allowed directory that points outside it.
        std::os::unix::fs::symlink(
            fixture.home.join("private"),
            fixture.home.join("src/escape"),
        )
        .unwrap();
        for root in ["~/private", "~/src/escape", "~/src/app/../../private"] {
            let error = message(fixture.resolve(roots(&["~/src/app", root]), &bounds));
            assert!(error.contains("outside the directories"), "{root}: {error}");
        }
        // An allowed directory that does not exist allows nothing.
        let nowhere = ShellBounds {
            allowed_roots: Some(vec!["~/elsewhere".into()]),
            ..ShellBounds::default()
        };
        assert!(fixture.resolve(roots(&["~/src/app"]), &nowhere).is_err());
    }

    #[test]
    fn grants_network_and_secrets_need_the_operator_to_allow_them() {
        let fixture = Fixture::new();
        let ask = |change: fn(&mut IsolatedShell)| {
            let mut request = roots(&["~/src/app"]);
            change(&mut request);
            request
        };
        // With nothing allowed, each request refuses the session.
        let nothing = ShellBounds::default();
        for (request, expected) in [
            (
                ask(|r| r.read_only = vec!["~/tools".into()]),
                "outside what this session host allows",
            ),
            (
                ask(|r| r.network = true),
                "does not allow isolated shells to use the network",
            ),
            (
                ask(|r| r.secrets = vec!["GH_TOKEN".into()]),
                "no secret named",
            ),
        ] {
            let error = message(fixture.resolve(request, &nothing));
            assert!(error.contains(expected), "{error}");
        }

        let generous = ShellBounds {
            allowed_read_only: vec!["~/tools".into()],
            allow_network: true,
            secrets: BTreeMap::from([(
                "GH_TOKEN".to_owned(),
                SecretSource {
                    env: "XSHELL_SECRET_GH_TOKEN".into(),
                },
            )]),
            ..ShellBounds::default()
        };
        generous.validate().unwrap();
        let mut request = roots(&["~/src/app"]);
        request.read_only = vec!["~/tools/bin/cc".into(), "~/tools".into()];
        request.network = true;
        request.secrets = vec!["GH_TOKEN".into()];
        request.user_commands = true;
        let ShellContainment::Isolated(isolated) = fixture.resolve(request, &generous).unwrap()
        else {
            panic!("expected an isolated shell");
        };
        assert_eq!(
            isolated.read_only,
            [
                fixture.home.join("tools/bin/cc"),
                fixture.home.join("tools")
            ]
        );
        assert!(isolated.network && isolated.user_commands);
        assert_eq!(isolated.secrets, ["GH_TOKEN"]);

        // What the operator allows and the session does not ask for is not
        // given.
        let ShellContainment::Isolated(modest) =
            fixture.resolve(roots(&["~/src/app"]), &generous).unwrap()
        else {
            panic!("expected an isolated shell");
        };
        assert!(!modest.network && modest.read_only.is_empty() && modest.secrets.is_empty());

        // A grant outside the allowed list, a protected one, and a repeated
        // secret are refused even here.
        for (request, expected) in [
            (
                ask(|r| r.read_only = vec!["~/private".into()]),
                "outside what",
            ),
            (
                ask(|r| r.secrets = vec!["GH_TOKEN".into(), "GH_TOKEN".into()]),
                "listed twice",
            ),
        ] {
            let error = message(fixture.resolve(request, &generous));
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn the_bounds_themselves_are_checked() {
        let strict_without_roots = ShellBounds {
            minimum: MinimumContainment::Isolated,
            ..ShellBounds::default()
        };
        assert!(
            strict_without_roots
                .validate()
                .unwrap_err()
                .to_string()
                .contains("needs session_fabric.shell.allowed_roots")
        );
        for (name, variable) in [("GH TOKEN", "OK"), ("OK", "9LIVES"), ("", "OK")] {
            let bounds = ShellBounds {
                secrets: BTreeMap::from([(
                    name.to_owned(),
                    SecretSource {
                        env: variable.into(),
                    },
                )]),
                ..ShellBounds::default()
            };
            assert!(bounds.validate().is_err(), "{name:?} {variable:?}");
        }
    }

    #[test]
    fn the_wire_form_omits_defaults_and_refuses_unknown_fields() {
        let minimal = ShellContainment::Isolated(roots(&["/srv/work"]));
        assert_eq!(
            serde_json::to_string(&minimal).unwrap(),
            r#"{"isolated":{"roots":["/srv/work"]}}"#
        );
        assert_eq!(
            serde_json::to_string(&ShellContainment::Ambient).unwrap(),
            "\"ambient\""
        );
        let parsed: ShellContainment =
            serde_json::from_str(r#"{"isolated":{"roots":["/a"],"network":true,"secrets":["T"]}}"#)
                .unwrap();
        let ShellContainment::Isolated(isolated) = parsed else {
            panic!("expected an isolated shell");
        };
        assert!(isolated.network && !isolated.user_commands);
        assert!(
            serde_json::from_str::<ShellContainment>(
                r#"{"isolated":{"roots":["/a"],"sudo":true}}"#
            )
            .is_err()
        );
    }
}

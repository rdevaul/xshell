//! Paths whose contents an agent should not read without a human decision.
//!
//! Read-only tools run automatically inside the working directory. That is
//! convenient until the directory holds `.env`, a private key, or a Git
//! config with an embedded token — none of which the model needs to "explain
//! this project". Matching paths are *promoted to approval-gated*, not
//! denied: `--approval auto` still reads them, `ask` prompts, `off` denies.
//!
//! Matching is done on the canonical path relative to the tool root, so a
//! symlink or `..` cannot dodge a pattern, and on the file name alone, so a
//! pattern like `*.pem` applies at any depth.

use std::path::Path;

/// Default patterns. Kept deliberately narrow: things that are almost always
/// credentials or keys, never general source or config.
pub const DEFAULT_SENSITIVE_PATTERNS: &[&str] = &[
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "*.jks",
    "*.keystore",
    "*.kdbx",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "*.ppk",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".git/config",
    ".ssh/**",
    ".aws/**",
    ".gnupg/**",
    ".kube/config",
    ".docker/config.json",
    "*.tfstate",
    "*.tfstate.*",
    "secrets.*",
    "*.secret",
    "*.secrets",
];

#[derive(Debug, Clone)]
pub struct SensitivePaths {
    patterns: Vec<String>,
}

impl Default for SensitivePaths {
    fn default() -> Self {
        Self::new(DEFAULT_SENSITIVE_PATTERNS.iter().map(|s| (*s).to_owned()))
    }
}

impl SensitivePaths {
    pub fn new(patterns: impl IntoIterator<Item = String>) -> Self {
        Self {
            patterns: patterns
                .into_iter()
                .map(|p| p.trim().trim_start_matches("./").to_owned())
                .filter(|p| !p.is_empty())
                .collect(),
        }
    }

    /// No patterns: nothing is ever promoted.
    pub fn none() -> Self {
        Self {
            patterns: Vec::new(),
        }
    }

    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// Does `relative` (a canonical path relative to the tool root, using `/`
    /// separators) match any pattern?
    pub fn matches(&self, relative: &Path) -> bool {
        let relative = relative
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let relative = relative.trim_start_matches("./");
        let file_name = relative.rsplit('/').next().unwrap_or(relative);
        self.patterns.iter().any(|pattern| {
            if pattern.contains('/') {
                glob_match(pattern, relative)
            } else {
                glob_match(pattern, file_name)
            }
        })
    }
}

/// Minimal glob: `*` matches within one path segment, `**` matches across
/// segments (including zero), `?` matches one non-separator character. No
/// character classes or escapes; patterns are operator-supplied config, not
/// untrusted input.
fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match p {
            [] => t.is_empty(),
            // `/**` at the end: match the directory itself or anything below.
            [b'/', b'*', b'*'] => t.is_empty() || t[0] == b'/',
            // `/**/`: match one `/` plus zero or more whole segments.
            [b'/', b'*', b'*', b'/', rest @ ..] => {
                if t.first() != Some(&b'/') {
                    return false;
                }
                let t = &t[1..];
                if go(rest, t) {
                    return true;
                }
                t.iter()
                    .enumerate()
                    .any(|(i, c)| *c == b'/' && go(rest, &t[i + 1..]))
            }
            // Leading `**/`: zero or more whole segments.
            [b'*', b'*', b'/', rest @ ..] => {
                if go(rest, t) {
                    return true;
                }
                t.iter()
                    .enumerate()
                    .any(|(i, c)| *c == b'/' && go(rest, &t[i + 1..]))
            }
            // Bare `**`: anything at all.
            [b'*', b'*'] => true,
            [b'*', rest @ ..] => {
                let mut i = 0;
                loop {
                    if go(rest, &t[i..]) {
                        return true;
                    }
                    if i >= t.len() || t[i] == b'/' {
                        return false;
                    }
                    i += 1;
                }
            }
            [b'?', rest @ ..] => match t {
                [c, tail @ ..] if *c != b'/' => go(rest, tail),
                _ => false,
            },
            [c, rest @ ..] => match t {
                [d, tail @ ..] if c == d => go(rest, tail),
                _ => false,
            },
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, t: &str) -> bool {
        glob_match(p, t)
    }

    #[test]
    fn glob_semantics() {
        assert!(m("*.pem", "server.pem"));
        assert!(!m("*.pem", "a/server.pem")); // `*` stops at `/`
        assert!(m(".env", ".env"));
        assert!(!m(".env", ".envrc"));
        assert!(m(".env.*", ".env.local"));
        assert!(m(".ssh/**", ".ssh/id_ed25519"));
        assert!(m(".ssh/**", ".ssh/deep/er/file"));
        assert!(m(".ssh/**", ".ssh")); // zero segments
        assert!(m("**/secrets.yaml", "secrets.yaml"));
        assert!(m("**/secrets.yaml", "a/b/secrets.yaml"));
        assert!(m("a/**/z", "a/z"));
        assert!(m("a/**/z", "a/b/c/z"));
        assert!(!m("a/**/z", "ab/z"));
        assert!(!m(".ssh/**", ".sshx"));
        assert!(!m(".ssh/**", ".sshx/file"));
        assert!(m("id_?sa", "id_rsa"));
        assert!(!m("id_?sa", "id_/sa"));
        assert!(m(".git/config", ".git/config"));
        assert!(!m(".git/config", "sub/.git/config")); // anchored when it has `/`
    }

    #[test]
    fn defaults_catch_common_secrets_and_spare_source() {
        let s = SensitivePaths::default();
        for hit in [
            ".env",
            ".env.production",
            "certs/server.key",
            "deploy/id_ed25519",
            ".ssh/config",
            ".aws/credentials",
            ".git/config",
            "infra/terraform.tfstate",
            "config/secrets.yaml",
        ] {
            assert!(s.matches(Path::new(hit)), "{hit} should be sensitive");
        }
        for miss in [
            "README.md",
            "src/main.rs",
            "Cargo.toml",
            ".gitignore",
            "env.example",
            "environment.yml",
            "sub/.git/HEAD",
            "keyboard.rs",
        ] {
            assert!(
                !s.matches(Path::new(miss)),
                "{miss} should not be sensitive"
            );
        }
    }

    #[test]
    fn custom_and_empty_pattern_sets() {
        let custom = SensitivePaths::new(vec!["*.sqlite".to_owned(), " ./notes/** ".to_owned()]);
        assert!(custom.matches(Path::new("data/app.sqlite")));
        assert!(custom.matches(Path::new("notes/private.md")));
        assert!(!custom.matches(Path::new(".env")));
        assert!(!SensitivePaths::none().matches(Path::new(".env")));
    }
}

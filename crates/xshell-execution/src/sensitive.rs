//! Paths whose contents an agent should not read without a human decision.
//!
//! Read-only tools run automatically inside the working directory. That is
//! convenient until the directory holds `.env`, a private key, or a Git
//! config with an embedded token — none of which the model needs to "explain
//! this project". Matching paths are *promoted to approval-gated*, not
//! denied: `--approval auto` still reads them, `ask` prompts, `off` denies.
//!
//! Matching is done on the canonical absolute path of the object a tool is
//! about to read (see `crate::prepared`), so a symlink or `..` cannot dodge a
//! pattern and the answer does not depend on the session's cwd:
//!
//! - a pattern without `/` matches the final component, so `*.pem` applies to
//!   a file of that name at any depth;
//! - a pattern with `/` matches at any depth of the path, so `.git/config`
//!   covers a nested repository's config and `.aws/**` covers everything
//!   under any `.aws` directory, including a project kept inside one;
//! - a pattern that starts with `/` is anchored to the filesystem root, for
//!   administrators who mean one specific location.
//!
//! Matching ignores letter case (finding F13). The default macOS volume is
//! case-insensitive, so `.ENV`, `.SSH/ID_ED25519` and `server.PEM` open the
//! same objects as their lowercase spellings, and so do spellings that differ
//! by more than ASCII case: APFS folds `ſ` (long s) to `s`, the Kelvin sign
//! to `k`, `ß` to `ss` and `ﬁ` to `fi`. Both pattern and path are case-folded
//! before they are compared ([`fold_case`]), on every platform and volume:
//! folding can only make more paths sensitive, which is the safe direction,
//! and on a case-sensitive volume it means `.ENV` is gated like `.env` even
//! though they are different files. `crate::prepared` also records the names
//! the filesystem stores where it can (macOS), so the path classified is the
//! path on disk whichever way the caller spelled it.
//!
//! Allow rules must not become easier to satisfy, so the approval policy
//! matches its `always_allowed` and `allowed` path patterns case-sensitively
//! ([`SensitivePaths::matches_exact`]); see `crate::policy`.

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

    /// Does `path` match any pattern, ignoring case? Callers pass the
    /// canonical absolute path of the object; a relative path is matched the
    /// same way, from its first component. Use this wherever a match
    /// restricts (sensitive, blocked and denied paths).
    pub fn matches(&self, path: &Path) -> bool {
        self.matches_with(path, true)
    }

    /// Does `path` match any pattern exactly as spelled? For rules where a
    /// match grants something (allow lists), so that case folding never makes
    /// a grant reach a different object on a case-sensitive volume.
    pub fn matches_exact(&self, path: &Path) -> bool {
        self.matches_with(path, false)
    }

    fn matches_with(&self, path: &Path, fold: bool) -> bool {
        let path = path
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let path = if fold { fold_case(&path) } else { path };
        let path = path.trim_start_matches("./").trim_start_matches('/');
        let file_name = path.rsplit('/').next().unwrap_or(path);
        self.patterns.iter().any(|pattern| {
            let folded;
            let pattern = if fold {
                folded = fold_case(pattern);
                &folded
            } else {
                pattern
            };
            if let Some(anchored) = pattern.strip_prefix('/') {
                glob_match(anchored, path)
            } else if pattern.contains('/') {
                // Leading `**/` matches zero or more whole segments.
                glob_match(&format!("**/{pattern}"), path)
            } else {
                glob_match(pattern, file_name)
            }
        })
    }
}

/// Case-fold `text` for comparison: Unicode lowercase, plus the full case
/// foldings that turn a non-ASCII character into ASCII letters, which
/// lowercasing alone misses (`ſ` is already lowercase, `ß` lowercases to
/// itself). Those are the spellings that would let a name such as
/// `ſerver.key` or `.ſsh/id_rsa` open a file matched by an ASCII pattern on a
/// case-insensitive volume. Folding never changes `/`, `*` or `?`.
pub(crate) fn fold_case(text: &str) -> String {
    let mut folded = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\u{17F}' => folded.push('s'), // ſ LATIN SMALL LETTER LONG S
            '\u{DF}' | '\u{1E9E}' => folded.push_str("ss"), // ß, ẞ
            '\u{FB00}' => folded.push_str("ff"), // ﬀ
            '\u{FB01}' => folded.push_str("fi"), // ﬁ
            '\u{FB02}' => folded.push_str("fl"), // ﬂ
            '\u{FB03}' => folded.push_str("ffi"), // ﬃ
            '\u{FB04}' => folded.push_str("ffl"), // ﬄ
            '\u{FB05}' | '\u{FB06}' => folded.push_str("st"), // ﬅ, ﬆ
            // Includes the Kelvin sign, which lowercases to `k`.
            _ => folded.extend(c.to_lowercase()),
        }
    }
    folded
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
    fn f4_slash_patterns_match_at_any_depth_of_the_absolute_path() {
        let s = SensitivePaths::default();
        // The same files from absolute paths, whatever the session cwd was.
        for hit in [
            "/home/user/work/.git/config",
            "/home/user/work/sub/.git/config",
            "/home/user/.aws/credentials",
            "/home/user/.aws/project/main.tf",
            "/home/user/.ssh",
            "/srv/repo/.env",
            "sub/.git/config",
        ] {
            assert!(s.matches(Path::new(hit)), "{hit} should be sensitive");
        }
        for miss in [
            "/home/user/work/README.md",
            "/home/user/work/.git/HEAD",
            "/home/user/.awsome/notes.md",
            "/home/user/git/config.rs",
        ] {
            assert!(
                !s.matches(Path::new(miss)),
                "{miss} should not be sensitive"
            );
        }
    }

    #[test]
    fn leading_slash_anchors_a_pattern_to_the_filesystem_root() {
        let anchored = SensitivePaths::new(vec!["/srv/vault/**".to_owned()]);
        assert!(anchored.matches(Path::new("/srv/vault/keys/a")));
        assert!(!anchored.matches(Path::new("/home/user/srv/vault/keys/a")));
        let floating = SensitivePaths::new(vec!["srv/vault/**".to_owned()]);
        assert!(floating.matches(Path::new("/home/user/srv/vault/keys/a")));
    }

    /// F13: on a case-insensitive volume each of these opens the canonical
    /// file, so each must classify the same. This checks the matching logic
    /// alone, so it runs on every platform.
    #[test]
    fn f13_matching_ignores_case_and_folds_like_a_case_insensitive_volume() {
        let s = SensitivePaths::default();
        for (variant, canonical) in [
            (".ENV", ".env"),
            (".Env.Production", ".env.production"),
            (".SSH/ID_ED25519", ".ssh/id_ed25519"),
            ("/home/user/.SSH", "/home/user/.ssh"),
            ("deploy/server.PEM", "deploy/server.pem"),
            (".GIT/config", ".git/config"),
            ("/srv/repo/sub/.Git/CONFIG", "/srv/repo/sub/.git/config"),
            ("/home/user/.AWS/credentials", "/home/user/.aws/credentials"),
            ("ID_RSA", "id_rsa"),
            // APFS also folds these to ASCII (checked on macOS 26).
            ("\u{17F}erver.key", "server.key"),  // long s
            ("server.\u{212A}ey", "server.key"), // Kelvin sign
            (".\u{DF}h/config", ".ssh/config"),
            ("\u{FB01}le.secret", "file.secret"),
            (".\u{17F}sh/id_ed25519", ".ssh/id_ed25519"),
        ] {
            assert!(s.matches(Path::new(canonical)), "{canonical} is sensitive");
            assert!(
                s.matches(Path::new(variant)),
                "f13: {variant} must be sensitive like {canonical}"
            );
        }
        // Folding a pattern folds it too, and spares unrelated names.
        let upper = SensitivePaths::new(vec!["SECRETS/**".to_owned()]);
        assert!(upper.matches(Path::new("/w/secrets/a")));
        for miss in ["README.MD", "SRC/MAIN.RS", ".GITIGNORE", "KEYBOARD.RS"] {
            assert!(
                !s.matches(Path::new(miss)),
                "{miss} should not be sensitive"
            );
        }
        // Exact matching, for allow rules, does not fold.
        let env = SensitivePaths::new(vec![".env".to_owned()]);
        assert!(env.matches_exact(Path::new("/w/.env")));
        assert!(!env.matches_exact(Path::new("/w/.ENV")));
        assert!(env.matches(Path::new("/w/.ENV")));
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

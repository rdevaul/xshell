use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const CONTROL_COMMANDS: &[&str] = &[
    "//agent", "//audit", "//help", "//model", "//quit", "//status", "//tools",
];

/// Sub-commands recognised after `//model`.
const MODEL_SUBCOMMANDS: &[&str] = &["list", "show", "use"];

pub struct XshellHelper {
    cwd: PathBuf,
    commands: Vec<String>,
    model_profiles: Vec<String>,
}

impl XshellHelper {
    pub fn new(cwd: PathBuf, model_profiles: Vec<String>) -> Self {
        Self {
            cwd,
            commands: discover_commands(),
            model_profiles,
        }
    }

    pub fn set_cwd(&mut self, cwd: PathBuf) {
        self.cwd = cwd;
    }

    #[allow(dead_code)]
    pub fn set_model_profiles(&mut self, profiles: Vec<String>) {
        self.model_profiles = profiles;
    }
}

impl Helper for XshellHelper {}
impl Hinter for XshellHelper {
    type Hint = String;
}
impl Highlighter for XshellHelper {}
impl Validator for XshellHelper {}

impl Completer for XshellHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _context: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Self::Candidate>)> {
        // --- Control commands ---
        if line[..pos].starts_with("//") {
            let fragment = &line[..pos];

            // Model profile completion: "//model <prefix>" or "//model use <prefix>"
            if let Some(result) = complete_model(line, pos, &self.model_profiles) {
                return Ok(result);
            }

            // Generic control command completion
            let matches = CONTROL_COMMANDS
                .iter()
                .filter(|command| command.starts_with(fragment))
                .map(|command| Pair {
                    display: (*command).into(),
                    replacement: (*command).into(),
                })
                .collect();
            return Ok((0, matches));
        }

        // --- Shell commands ---
        if !line[..pos].starts_with('$') {
            return Ok((pos, Vec::new()));
        }

        let start = line[..pos]
            .rfind(|character: char| character.is_whitespace() || "|&;<>".contains(character))
            .map_or(1, |index| index + 1);
        let fragment = &line[start..pos];
        let first_word = !line[1..start].chars().any(char::is_whitespace);

        if first_word && !fragment.contains('/') {
            let matches = self
                .commands
                .iter()
                .filter(|command| command.starts_with(fragment))
                .map(|command| Pair {
                    display: command.clone(),
                    replacement: command.clone(),
                })
                .collect();
            return Ok((start, matches));
        }

        Ok((start, complete_path(fragment, &self.cwd)))
    }
}

/// Attempt model-profile completion for lines that begin with `//model`.
/// Returns `Some((start, candidates))` when the line matches the pattern,
/// otherwise `None` so the caller can fall through to generic control
/// command completion.
fn complete_model(
    line: &str,
    pos: usize,
    profiles: &[String],
) -> Option<(usize, Vec<Pair>)> {
    let prefix = &line[..pos];

    // "//model" alone (possibly followed by whitespace)
    if prefix == "//model" || prefix.starts_with("//model ") {
        let after = &prefix["//model".len()..];
        // Strip leading space(s) to find what the user typed after `//model `
        let trimmed = after.trim_start();
        let space_offset = after.len() - trimmed.len();

        if trimmed.is_empty() {
            // "//model " — offer subcommands + all profiles
            let start = "//model".len();
            let mut candidates: Vec<Pair> = MODEL_SUBCOMMANDS
                .iter()
                .map(|sub| Pair {
                    display: format!("{sub} "),
                    replacement: sub.to_string(),
                })
                .chain(profiles.iter().map(|p| Pair {
                    display: p.clone(),
                    replacement: p.clone(),
                }))
                .collect();
            candidates.sort_by(|a, b| a.display.cmp(&b.display));
            return Some((start, candidates));
        }

        // "//model use <prefix>" — offer only profile names
        if let Some(rest) = trimmed.strip_prefix("use") {
            let after_use = rest.trim_start();
            let is_exact = trimmed == "use";
            if is_exact || after_use.is_empty() || rest.starts_with(' ') {
                let start = "//model use".len() + space_offset;
                let candidates = profiles
                    .iter()
                    .filter(|p| is_exact || p.starts_with(after_use))
                    .map(|p| Pair {
                        display: p.clone(),
                        replacement: p.clone(),
                    })
                    .collect();
                return Some((start, candidates));
            }
            // If they typed "//model usefoo" (no space), treat as a
            // subcommand match attempt — fall through to generic logic
            // below which will find no match and return to caller.
        }

        // "//model list" or "//model show" — no further completion
        if trimmed == "list" || trimmed == "show" {
            return None;
        }

        // "//model <partial-subcommand-or-profile>"
        let is_subcommand = trimmed == "use"
            || trimmed.starts_with("use")
            || trimmed == "list"
            || trimmed.starts_with("list")
            || trimmed == "show"
            || trimmed.starts_with("show");

        if !is_subcommand {
            // Treat as a profile-name prefix
            let start = "//model".len() + space_offset;
            let candidates = profiles
                .iter()
                .filter(|p| p.starts_with(trimmed))
                .map(|p| Pair {
                    display: p.clone(),
                    replacement: p.clone(),
                })
                .collect();
            return Some((start, candidates));
        }
    }

    None
}

fn discover_commands() -> Vec<String> {
    let mut commands = BTreeSet::from(["cd".to_owned()]);
    let Some(path) = env::var_os("PATH") else {
        return commands.into_iter().collect();
    };
    for directory in env::split_paths(&path) {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_file()
                && metadata.permissions().mode() & 0o111 != 0
                && let Some(name) = entry.file_name().to_str()
            {
                commands.insert(name.to_owned());
            }
        }
    }
    commands.into_iter().collect()
}

fn complete_path(fragment: &str, cwd: &Path) -> Vec<Pair> {
    let fragment = fragment.replace("\\ ", " ");
    let (text_directory, name_prefix) = fragment
        .rfind('/')
        .map_or(("", fragment.as_str()), |slash| {
            (&fragment[..=slash], &fragment[slash + 1..])
        });
    let search_directory = if text_directory.is_empty() {
        cwd.to_owned()
    } else if let Some(rest) = text_directory.strip_prefix("~/") {
        match env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => return Vec::new(),
        }
    } else if Path::new(text_directory).is_absolute() {
        PathBuf::from(text_directory)
    } else {
        cwd.join(text_directory)
    };

    let Ok(entries) = fs::read_dir(search_directory) else {
        return Vec::new();
    };
    let mut matches = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.starts_with(name_prefix)
                || (name.starts_with('.') && !name_prefix.starts_with('.'))
            {
                return None;
            }
            let suffix = if entry.file_type().ok()?.is_dir() {
                "/"
            } else {
                ""
            };
            let replacement = format!("{}{}{}", text_directory, name.replace(' ', "\\ "), suffix);
            Some(Pair {
                display: format!("{name}{suffix}"),
                replacement,
            })
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.display.cmp(&right.display));
    matches
}
#[cfg(test)]
mod tests {
    use super::*;
    use rustyline::history::DefaultHistory;

    #[test]
    fn path_completion_uses_xshell_cwd() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let matches = complete_path("Cargo", root);
        assert!(matches.iter().any(|pair| pair.replacement == "Cargo.toml"));
    }

    fn helper_with_profiles() -> XshellHelper {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        XshellHelper::new(
            root.to_owned(),
            vec![
                "local-qwen".to_owned(),
                "openrouter-free".to_owned(),
                "openai".to_owned(),
            ],
        )
    }

    #[test]
    fn completes_control_commands() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let helper = XshellHelper::new(root.to_owned(), Vec::new());
        let history = DefaultHistory::new();
        let context = Context::new(&history);
        let (start, matches) = helper.complete("//st", 4, &context).unwrap();
        assert_eq!(start, 0);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].replacement, "//status");
    }

    #[test]
    fn completes_model_subcommands_and_profiles() {
        let helper = helper_with_profiles();
        let history = DefaultHistory::new();
        let context = Context::new(&history);
        let (start, matches) = helper.complete("//model ", 8, &context).unwrap();
        assert_eq!(start, 7);
        let displays: Vec<_> = matches.iter().map(|p| p.display.as_str()).collect();
        assert!(displays.iter().any(|d| d.starts_with("list")));
        assert!(displays.iter().any(|d| d.starts_with("show")));
        assert!(displays.iter().any(|d| d.starts_with("use")));
        assert!(displays.iter().any(|d| *d == "local-qwen"));
        assert!(displays.iter().any(|d| *d == "openrouter-free"));
        assert!(displays.iter().any(|d| *d == "openai"));
    }

    #[test]
    fn completes_model_use_profiles() {
        let helper = helper_with_profiles();
        let history = DefaultHistory::new();
        let context = Context::new(&history);
        let (_, matches) = helper.complete("//model use ", 12, &context).unwrap();
        let names: Vec<_> = matches.iter().map(|p| p.replacement.as_str()).collect();
        assert_eq!(names, vec!["local-qwen", "openrouter-free", "openai"]);
    }

    #[test]
    fn completes_model_use_with_prefix() {
        let helper = helper_with_profiles();
        let history = DefaultHistory::new();
        let context = Context::new(&history);
        let (_, matches) = helper.complete("//model use open", 16, &context).unwrap();
        let names: Vec<_> = matches.iter().map(|p| p.replacement.as_str()).collect();
        assert!(names.iter().any(|n| n.starts_with("open")));
        assert!(!names.contains(&"local-qwen"));
    }

    #[test]
    fn completes_partial_profile_after_model() {
        let helper = helper_with_profiles();
        let history = DefaultHistory::new();
        let context = Context::new(&history);
        let (start, matches) = helper.complete("//model loc", 11, &context).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].replacement, "local-qwen");
        assert_eq!(start, 8);
    }
}

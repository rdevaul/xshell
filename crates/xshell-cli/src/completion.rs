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
    "//agent",
    "//audit",
    "//close",
    "//detach",
    "//help",
    "//model",
    "//new",
    "//quit",
    "//sessions",
    "//status",
    "//switch",
    "//tools",
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
fn complete_model(line: &str, pos: usize, profiles: &[String]) -> Option<(usize, Vec<Pair>)> {
    let prefix = &line[..pos];
    const COMMAND: &str = "//model";
    let rest = prefix.strip_prefix(COMMAND)?;

    // With no separator yet, preserve the command and insert one as part of
    // every replacement. This makes `//model<Tab>` produce a valid command.
    if rest.is_empty() {
        return Some((COMMAND.len(), model_root_candidates("", profiles, " ")));
    }
    if !rest.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }

    let arguments = rest.trim_start_matches(char::is_whitespace);
    let arguments_start = prefix.len() - arguments.len();
    if arguments.is_empty() {
        // Keep existing whitespace and insert after it.
        return Some((pos, model_root_candidates("", profiles, "")));
    }

    let Some(first_separator) = arguments.find(char::is_whitespace) else {
        return match arguments {
            "list" | "show" => Some((pos, Vec::new())),
            "use" => Some((pos, profile_candidates("", profiles, " "))),
            partial => Some((
                arguments_start,
                model_root_candidates(partial, profiles, ""),
            )),
        };
    };

    let first_argument = &arguments[..first_separator];
    if first_argument != "use" {
        return Some((pos, Vec::new()));
    }

    let after_first = &arguments[first_separator..];
    let profile_prefix = after_first.trim_start_matches(char::is_whitespace);
    if profile_prefix.contains(char::is_whitespace) {
        return Some((pos, Vec::new()));
    }
    let profile_start = prefix.len() - profile_prefix.len();
    Some((
        profile_start,
        profile_candidates(profile_prefix, profiles, ""),
    ))
}

fn model_root_candidates(prefix: &str, profiles: &[String], replacement_prefix: &str) -> Vec<Pair> {
    let mut candidates: Vec<Pair> = MODEL_SUBCOMMANDS
        .iter()
        .filter(|subcommand| subcommand.starts_with(prefix))
        .map(|subcommand| Pair {
            display: if *subcommand == "use" {
                "use ".into()
            } else {
                (*subcommand).into()
            },
            replacement: format!(
                "{replacement_prefix}{subcommand}{}",
                if *subcommand == "use" { " " } else { "" }
            ),
        })
        .chain(
            profiles
                .iter()
                .filter(|profile| profile.starts_with(prefix))
                .map(|profile| Pair {
                    display: profile.clone(),
                    replacement: format!("{replacement_prefix}{profile}"),
                }),
        )
        .collect();
    candidates.sort_by(|left, right| left.display.cmp(&right.display));
    candidates
}

fn profile_candidates(prefix: &str, profiles: &[String], replacement_prefix: &str) -> Vec<Pair> {
    profiles
        .iter()
        .filter(|profile| profile.starts_with(prefix))
        .map(|profile| Pair {
            display: profile.clone(),
            replacement: format!("{replacement_prefix}{profile}"),
        })
        .collect()
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

    fn apply_completion(line: &str, pos: usize, start: usize, replacement: &str) -> String {
        format!("{}{}{}", &line[..start], replacement, &line[pos..])
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
        assert_eq!(start, 8);
        let displays: Vec<_> = matches.iter().map(|p| p.display.as_str()).collect();
        assert!(displays.iter().any(|d| d.starts_with("list")));
        assert!(displays.iter().any(|d| d.starts_with("show")));
        assert!(displays.iter().any(|d| d.starts_with("use")));
        assert!(displays.contains(&"local-qwen"));
        assert!(displays.contains(&"openrouter-free"));
        assert!(displays.contains(&"openai"));
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

    #[test]
    fn model_completion_inserts_a_missing_space() {
        let helper = helper_with_profiles();
        let history = DefaultHistory::new();
        let context = Context::new(&history);
        let line = "//model";
        let (start, matches) = helper.complete(line, line.len(), &context).unwrap();
        let profile = matches
            .iter()
            .find(|candidate| candidate.display == "local-qwen")
            .unwrap();

        assert_eq!(start, line.len());
        assert_eq!(
            apply_completion(line, line.len(), start, &profile.replacement),
            "//model local-qwen"
        );
    }

    #[test]
    fn model_completion_preserves_an_existing_space() {
        let helper = helper_with_profiles();
        let history = DefaultHistory::new();
        let context = Context::new(&history);
        let line = "//model ";
        let (start, matches) = helper.complete(line, line.len(), &context).unwrap();
        let profile = matches
            .iter()
            .find(|candidate| candidate.display == "local-qwen")
            .unwrap();

        assert_eq!(
            apply_completion(line, line.len(), start, &profile.replacement),
            "//model local-qwen"
        );
    }

    #[test]
    fn model_use_completion_inserts_a_missing_space() {
        let helper = helper_with_profiles();
        let history = DefaultHistory::new();
        let context = Context::new(&history);
        let line = "//model use";
        let (start, matches) = helper.complete(line, line.len(), &context).unwrap();
        let profile = &matches[0];

        assert_eq!(
            apply_completion(line, line.len(), start, &profile.replacement),
            "//model use local-qwen"
        );
    }
}

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
    "//agent", "//help", "//model", "//quit", "//status", "//tools",
];

pub struct XshellHelper {
    cwd: PathBuf,
    commands: Vec<String>,
}

impl XshellHelper {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            commands: discover_commands(),
        }
    }

    pub fn set_cwd(&mut self, cwd: PathBuf) {
        self.cwd = cwd;
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
        if line[..pos].starts_with("//") {
            let fragment = &line[..pos];
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

    #[test]
    fn completes_control_commands() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let helper = XshellHelper::new(root.to_owned());
        let history = DefaultHistory::new();
        let context = Context::new(&history);
        let (start, matches) = helper.complete("//st", 4, &context).unwrap();
        assert_eq!(start, 0);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].replacement, "//status");
    }
}

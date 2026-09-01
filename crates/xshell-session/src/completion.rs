use crate::{ShellCompletionCandidate, ShellCompletionResult};
use anyhow::{Result, bail};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const MAX_INPUT_BYTES: usize = 16 * 1024;
const MAX_CANDIDATES: usize = 256;
const MAX_SCANNED_ENTRIES: usize = 32 * 1024;
const MAX_CANDIDATE_BYTES: usize = 4 * 1024;
const COMMAND_CACHE_TTL: Duration = Duration::from_secs(30);
const SHELL_BUILTINS: &[&str] = &[
    "alias", "bg", "cd", "command", "echo", "eval", "exec", "export", "false", "fg", "jobs",
    "kill", "printf", "pwd", "read", "set", "test", "true", "type", "ulimit", "umask", "unalias",
    "unset", "wait",
];

/// Complete only executable names and filesystem paths. This deliberately does
/// not invoke a shell, source startup files, expand variables, or evaluate any
/// part of the input line.
pub fn complete_shell(line: &str, cursor: usize, cwd: &Path) -> Result<ShellCompletionResult> {
    if line.len() > MAX_INPUT_BYTES {
        bail!("completion input exceeds {MAX_INPUT_BYTES} bytes");
    }
    if cursor > line.len() || !line.is_char_boundary(cursor) {
        bail!("completion cursor is not a valid UTF-8 boundary");
    }
    let prefix = &line[..cursor];
    let shell_start = if prefix.starts_with("$$") {
        2
    } else if prefix.starts_with('$') {
        1
    } else {
        return Ok(ShellCompletionResult {
            start: cursor,
            candidates: Vec::new(),
        });
    };
    let shell_prefix = &prefix[shell_start..];
    let relative_start = shell_prefix
        .rfind(|character: char| character.is_whitespace() || "|&;<>()".contains(character))
        .map_or(0, |index| index + 1);
    let start = shell_start + relative_start;
    let fragment = &prefix[start..];
    let before = shell_prefix[..relative_start].trim_end();
    let command_position = before.is_empty()
        || before.ends_with('|')
        || before.ends_with(';')
        || before.ends_with('&')
        || before.ends_with('(');

    let candidates = if command_position && !fragment.contains('/') {
        complete_command(fragment)
    } else {
        complete_path(fragment, cwd)
    };
    Ok(ShellCompletionResult { start, candidates })
}

fn complete_command(prefix: &str) -> Vec<ShellCompletionCandidate> {
    let commands = command_catalog();
    commands
        .iter()
        .filter(|command| command.starts_with(prefix))
        .take(MAX_CANDIDATES)
        .map(|command| ShellCompletionCandidate {
            display: command.clone(),
            replacement: escape_basic(command),
        })
        .collect()
}

fn command_catalog() -> Arc<Vec<String>> {
    struct CachedCommands {
        refreshed_at: Instant,
        commands: Arc<Vec<String>>,
    }

    static COMMANDS: OnceLock<Mutex<Option<CachedCommands>>> = OnceLock::new();
    let cache = COMMANDS.get_or_init(|| Mutex::new(None));
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if cache
        .as_ref()
        .is_none_or(|cached| cached.refreshed_at.elapsed() >= COMMAND_CACHE_TTL)
    {
        *cache = Some(CachedCommands {
            refreshed_at: Instant::now(),
            commands: Arc::new(scan_command_catalog()),
        });
    }
    cache
        .as_ref()
        .map(|cached| Arc::clone(&cached.commands))
        .unwrap_or_default()
}

fn scan_command_catalog() -> Vec<String> {
    let mut commands = SHELL_BUILTINS
        .iter()
        .map(|command| (*command).to_owned())
        .collect::<BTreeSet<_>>();
    let mut scanned = 0;
    if let Some(path) = env::var_os("PATH") {
        'directories: for directory in env::split_paths(&path) {
            let Ok(entries) = fs::read_dir(directory) else {
                continue;
            };
            for entry in entries.flatten() {
                scanned += 1;
                if scanned > MAX_SCANNED_ENTRIES {
                    break 'directories;
                }
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if name.len() > MAX_CANDIDATE_BYTES || name.chars().any(char::is_control) {
                    continue;
                }
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
                    commands.insert(name);
                }
            }
        }
    }
    commands.into_iter().collect()
}

fn complete_path(fragment: &str, cwd: &Path) -> Vec<ShellCompletionCandidate> {
    let (raw_text_directory, raw_name_prefix) =
        fragment.rfind('/').map_or(("", fragment), |slash| {
            (&fragment[..=slash], &fragment[slash + 1..])
        });
    let text_directory = unescape_basic(raw_text_directory);
    let name_prefix = unescape_basic(raw_name_prefix);
    let search_directory = if text_directory.is_empty() {
        cwd.to_owned()
    } else if let Some(rest) = text_directory.strip_prefix("~/") {
        match env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => return Vec::new(),
        }
    } else if Path::new(&text_directory).is_absolute() {
        PathBuf::from(text_directory)
    } else {
        cwd.join(text_directory)
    };

    let Ok(entries) = fs::read_dir(search_directory) else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    for entry in entries.flatten().take(MAX_SCANNED_ENTRIES) {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.len() > MAX_CANDIDATE_BYTES
            || name.chars().any(char::is_control)
            || !name.starts_with(&name_prefix)
            || (name.starts_with('.') && !name_prefix.starts_with('.'))
        {
            continue;
        }
        let suffix = if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            "/"
        } else {
            ""
        };
        candidates.push(ShellCompletionCandidate {
            display: format!("{name}{suffix}"),
            replacement: format!("{}{}{}", raw_text_directory, escape_basic(&name), suffix),
        });
    }
    candidates.sort_by(|left, right| left.display.cmp(&right.display));
    candidates.truncate(MAX_CANDIDATES);
    candidates
}

fn unescape_basic(fragment: &str) -> String {
    let mut value = String::with_capacity(fragment.len());
    let mut characters = fragment.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            if let Some(escaped) = characters.next() {
                value.push(escaped);
            }
        } else {
            value.push(character);
        }
    }
    value
}

fn escape_basic(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_alphanumeric() || "_-.,+@%".contains(character) {
            escaped.push(character);
        } else {
            escaped.push('\\');
            escaped.push(character);
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn completes_paths_without_evaluating_input() {
        let temporary = TempDir::new().unwrap();
        fs::write(temporary.path().join("hello world.txt"), "test").unwrap();
        fs::write(temporary.path().join("hello;$USER.txt"), "test").unwrap();
        fs::create_dir(temporary.path().join("hello-dir")).unwrap();
        let result = complete_shell("$cat hello", 10, temporary.path()).unwrap();
        assert_eq!(result.start, 5);
        assert!(
            result
                .candidates
                .iter()
                .any(|candidate| candidate.replacement == "hello\\ world.txt")
        );
        assert!(
            result
                .candidates
                .iter()
                .any(|candidate| candidate.replacement == "hello\\;\\$USER.txt")
        );
        assert!(
            result
                .candidates
                .iter()
                .any(|candidate| candidate.replacement == "hello-dir/")
        );
    }

    #[test]
    fn understands_sticky_prefix_and_pipeline_command_positions() {
        let result = complete_shell("$$printf hi | pw", 16, Path::new("/tmp")).unwrap();
        assert_eq!(result.start, 14);
        assert!(
            result
                .candidates
                .iter()
                .any(|candidate| candidate.replacement == "pwd")
        );
    }

    #[test]
    fn rejects_oversized_or_invalid_cursor_inputs() {
        assert!(complete_shell(&"x".repeat(MAX_INPUT_BYTES + 1), 0, Path::new(".")).is_err());
        assert!(complete_shell("$é", 2, Path::new(".")).is_err());
    }
}

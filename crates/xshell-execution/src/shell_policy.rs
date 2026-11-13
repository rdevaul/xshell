//! Reading shell commands for the approval policy (finding F19).
//!
//! A `run_shell` denial such as `run_shell: rm *` is only worth something if
//! the shell cannot be talked into running `rm` under another spelling. So
//! when a policy has shell denials, a command may run unattended only if this
//! module understands every program the command would start, and none of them
//! is denied. Anything it does not fully understand is *uncheckable* and is
//! never run unattended (see `Policy::decide`).
//!
//! What it understands:
//!
//! - **Commands, not text.** The command is split with the shell's quoting
//!   rules into simple commands at `;`, `&`, `&&`, `|`, `||`, `|&` and
//!   newlines. Redirections (`>`, `2>&1`, `<`) are attached to their command
//!   and their targets are not mistaken for commands. Anything that expands
//!   (`$`, backquotes, `(`, `)`, `{`, `}`), a here-document, or quoting that
//!   does not close makes the command uncheckable.
//! - **The program actually run.** A command word is resolved the way the
//!   shell would: a word with `/` against the working directory, a bare name
//!   through `PATH`. The program is identified by its own name, by the name
//!   of the file it resolves to after symlinks, and by that file's identity
//!   (device and inode, which catches hard links). Names are compared with
//!   case folded, so `RM` matches `rm` on a case-insensitive volume.
//! - **Transparent wrappers.** `command`, `exec`, `nohup`, `time -p` and
//!   `env` with only assignments, `-i` or `-u NAME` are stripped, and the
//!   program they run is checked instead.
//!
//! What makes a command uncheckable, when a denial exists ([`analyze`]
//! returns `None`):
//!
//! - shell reserved words and other wrappers that run a program the policy
//!   cannot see ([`OPAQUE`]): `sudo`, `xargs`, `coproc`, `repeat`, `eval`,
//!   `source`, `timeout`, `nice`, `setsid`, and the rest of that list;
//! - every shell ([`SHELLS`]), whatever its arguments, and other
//!   interpreters ([`INTERPRETERS`]) unless their first argument is an
//!   existing script file: `python -c`, `perl -e`, `node -e`, reading a
//!   program from standard input, `awk` without `-f`, and `sed` with its `e`
//!   command or `s///e` flag;
//! - `find` with `-exec`, `-execdir`, `-ok`, `-okdir` or `-delete`, and `git`
//!   with `-c` (which can define an alias that runs a shell command);
//! - in a compound command, anything that could change which program a later
//!   part runs: a name-binding or file-writing command ([`BINDERS`]: `hash`,
//!   `alias`, `ln`, `cp`, `curl`, ...) or an output redirection in any part
//!   but the last, and a command word with `/` in any part but the first.
//!
//! Residual limits: the policy reads the command, not the programs it runs.
//! A script, a Makefile, or any allowed program can still do internally what
//! a denied program would (H6). Names are resolved with the daemon's `PATH`
//! at decision time; a shell startup file that changes `PATH` or defines an
//! alias or function is not seen, and a copy (not a link) of a denied program
//! made by an earlier call is not recognised.

use crate::sensitive::fold_case;
use std::ffi::OsStr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// One word of a simple command, with whether it held unquoted glob or tilde
/// characters, which the shell would expand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Word {
    pub text: String,
    pub expands: bool,
}

/// One simple command: its words, and whether it redirects output to a file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Segment {
    pub words: Vec<Word>,
    pub writes_file: bool,
}

/// Characters that substitute or expand text anywhere in a command, so the
/// words the shell runs cannot be read from the command as written.
const EXPANSIONS: &[char] = &['`', '$', '(', ')', '{', '}'];

/// Split `command` into simple commands, or `None` if it uses syntax this
/// reader does not follow.
pub(crate) fn segments(command: &str) -> Option<Vec<Segment>> {
    if command.contains(EXPANSIONS) {
        return None;
    }
    let chars: Vec<char> = command.chars().collect();
    let mut segments = Vec::new();
    let mut current = Segment::default();
    let mut word = String::new();
    let mut in_word = false;
    let mut expands = false;
    let mut target_pending = false;
    let mut index = 0;

    // Finish the word being read: a redirection target, or a command word.
    let finish = |current: &mut Segment,
                  word: &mut String,
                  in_word: &mut bool,
                  expands: &mut bool,
                  target_pending: &mut bool| {
        if *in_word {
            if *target_pending {
                *target_pending = false;
            } else {
                current.words.push(Word {
                    text: std::mem::take(word),
                    expands: *expands,
                });
            }
        }
        word.clear();
        *in_word = false;
        *expands = false;
    };

    while index < chars.len() {
        let c = chars[index];
        match c {
            ' ' | '\t' => {
                finish(
                    &mut current,
                    &mut word,
                    &mut in_word,
                    &mut expands,
                    &mut target_pending,
                );
                index += 1;
            }
            '#' if !in_word => {
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                }
            }
            '\n' | ';' | '|' | '&' => {
                // `&>` and `&>>` redirect output; everything else separates.
                if c == '&' && chars.get(index + 1) == Some(&'>') {
                    finish(
                        &mut current,
                        &mut word,
                        &mut in_word,
                        &mut expands,
                        &mut target_pending,
                    );
                    index += 2;
                    if chars.get(index) == Some(&'>') {
                        index += 1;
                    }
                    current.writes_file = true;
                    target_pending = true;
                    continue;
                }
                if c == ';' && chars.get(index + 1) == Some(&';') {
                    return None; // `case` syntax
                }
                finish(
                    &mut current,
                    &mut word,
                    &mut in_word,
                    &mut expands,
                    &mut target_pending,
                );
                if target_pending {
                    return None;
                }
                segments.push(std::mem::take(&mut current));
                index += 1;
                // `&&`, `||` and `|&` are one operator.
                if matches!(
                    (c, chars.get(index)),
                    ('&', Some('&')) | ('|', Some('|')) | ('|', Some('&'))
                ) {
                    index += 1;
                }
            }
            '<' | '>' => {
                // A word of digits right before the operator is a descriptor.
                if in_word && !word.is_empty() && word.chars().all(|c| c.is_ascii_digit()) {
                    word.clear();
                    in_word = false;
                    expands = false;
                } else {
                    finish(
                        &mut current,
                        &mut word,
                        &mut in_word,
                        &mut expands,
                        &mut target_pending,
                    );
                }
                if target_pending {
                    return None;
                }
                index += 1;
                if c == '<' && chars.get(index) == Some(&'<') {
                    return None; // here-documents and here-strings
                }
                // Whether the target, once read, is a file opened for writing.
                let mut output = c == '>';
                match chars.get(index) {
                    Some('>' | '|') if c == '>' => index += 1,
                    Some('>') if c == '<' => {
                        output = true;
                        index += 1;
                    }
                    Some('&') => {
                        index += 1;
                        // `2>&1`, `>&-`: a descriptor, not a file.
                        let start = index;
                        while index < chars.len()
                            && (chars[index].is_ascii_digit() || chars[index] == '-')
                        {
                            index += 1;
                        }
                        if index > start {
                            continue;
                        }
                    }
                    _ => {}
                }
                if output {
                    current.writes_file = true;
                }
                target_pending = true;
            }
            '\'' => {
                in_word = true;
                index += 1;
                loop {
                    match chars.get(index) {
                        None => return None,
                        Some('\'') => break,
                        Some(&c) => word.push(c),
                    }
                    index += 1;
                }
                index += 1;
            }
            '"' => {
                in_word = true;
                index += 1;
                loop {
                    match chars.get(index) {
                        None => return None,
                        Some('"') => break,
                        Some('\\') => {
                            index += 1;
                            match chars.get(index) {
                                None => return None,
                                Some(&next @ ('"' | '\\' | '$' | '`')) => word.push(next),
                                Some('\n') => {}
                                Some(&next) => {
                                    word.push('\\');
                                    word.push(next);
                                }
                            }
                        }
                        Some(&c) => word.push(c),
                    }
                    index += 1;
                }
                index += 1;
            }
            '\\' => {
                index += 1;
                match chars.get(index) {
                    None => return None,
                    Some('\n') => {}
                    Some(&next) => {
                        in_word = true;
                        word.push(next);
                    }
                }
                index += 1;
            }
            _ => {
                if matches!(c, '*' | '?' | '[' | ']' | '~') {
                    expands = true;
                }
                in_word = true;
                word.push(c);
                index += 1;
            }
        }
    }
    finish(
        &mut current,
        &mut word,
        &mut in_word,
        &mut expands,
        &mut target_pending,
    );
    if target_pending {
        return None;
    }
    segments.push(current);
    // A leading `!` negates a pipeline; it runs nothing itself.
    for segment in &mut segments {
        if segment
            .words
            .first()
            .is_some_and(|word| word.text == "!" && !word.expands)
        {
            segment.words.remove(0);
        }
    }
    segments.retain(|segment| !segment.words.is_empty() || segment.writes_file);
    Some(segments)
}

/// Whether `word` is a plain command name or path, which the shell runs as
/// written. Rejects variable assignments (`FOO=1 rm`), globs (`r?`), tilde
/// and zsh `=` expansion (`=rm`), and anything else that could make the
/// shell run a command other than the one the word spells.
pub(crate) fn plain_command_word(word: &str) -> bool {
    !word.is_empty()
        && word.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '+' | ':' | '@' | ',')
        })
}

/// Shell reserved words and wrappers that run another program the policy
/// cannot see, or change how it runs.
pub(crate) const OPAQUE: &[&str] = &[
    // Reserved words and grammar.
    "if",
    "then",
    "else",
    "elif",
    "fi",
    "while",
    "until",
    "for",
    "in",
    "do",
    "done",
    "case",
    "esac",
    "select",
    "function",
    "coproc",
    "repeat",
    "foreach",
    "end",
    "[[",
    "]]",
    "nocorrect",
    // Builtins that run, load or schedule code.
    "builtin",
    "eval",
    "source",
    ".",
    "trap",
    "noglob",
    "fc",
    "sched",
    "zargs",
    "autoload",
    "zmodload",
    "enable",
    // Programs that run another program.
    "sudo",
    "doas",
    "su",
    "runuser",
    "pkexec",
    "nice",
    "renice",
    "ionice",
    "chrt",
    "taskset",
    "numactl",
    "timeout",
    "gtimeout",
    "stdbuf",
    "unbuffer",
    "setsid",
    "xargs",
    "parallel",
    "watch",
    "strace",
    "ltrace",
    "dtrace",
    "dtruss",
    "gdb",
    "lldb",
    "valgrind",
    "chroot",
    "unshare",
    "nsenter",
    "flock",
    "script",
    "expect",
    "fakeroot",
    "caffeinate",
    "sandbox-exec",
    "firejail",
    "bwrap",
    "systemd-run",
    "at",
    "batch",
    "crontab",
    "launchctl",
];

/// Shells: whatever their arguments, they run commands the policy cannot see.
pub(crate) const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "ksh", "mksh", "pdksh", "oksh", "yash", "fish", "tcsh", "csh",
    "ash", "busybox", "toybox", "rbash",
];

/// Interpreters: uncheckable unless their first argument is an existing
/// script file. Names that start with one of these (`python3.12`) count.
pub(crate) const INTERPRETERS: &[&str] = &[
    "python",
    "perl",
    "ruby",
    "node",
    "nodejs",
    "deno",
    "bun",
    "php",
    "lua",
    "luajit",
    "tclsh",
    "wish",
    "rscript",
    "osascript",
    "pwsh",
    "powershell",
    "jshell",
    "irb",
    "julia",
];

/// Commands that bind names or write files, and so can change which program
/// a later part of a compound command runs.
pub(crate) const BINDERS: &[&str] = &[
    "alias", "unalias", "hash", "rehash", "export", "typeset", "declare", "local", "readonly",
    "set", "unset", "setopt", "unsetopt", "shopt", "ln", "cp", "mv", "install", "rsync", "tee",
    "dd", "curl", "wget", "chmod", "chown", "tar", "unzip", "cpio", "patch", "ditto", "touch",
    "mkfifo",
];

/// Commands that change the working directory; they matter only when `PATH`
/// has a relative entry.
const DIRECTORY_CHANGERS: &[&str] = &["cd", "pushd", "popd"];

/// One program a command would run.
#[derive(Debug, Clone)]
pub(crate) struct Invocation {
    /// Its arguments, starting with the command word, after transparent
    /// wrappers are stripped.
    pub argv: Vec<String>,
    /// The folded names it answers to: the command word's own file name and
    /// the file name of what it resolves to.
    pub names: Vec<String>,
    /// The device and inode of the file it resolves to.
    pub identity: Option<(u64, u64)>,
}

/// Where a command's programs are looked up.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Lookup<'a> {
    pub cwd: Option<&'a Path>,
    pub search_path: Option<&'a OsStr>,
}

impl Lookup<'_> {
    fn path_entries(&self) -> Vec<PathBuf> {
        let process;
        let search = match self.search_path {
            Some(search) => search,
            None => {
                process = std::env::var_os("PATH").unwrap_or_default();
                process.as_os_str()
            }
        };
        std::env::split_paths(search).collect()
    }

    fn has_relative_entry(&self) -> bool {
        self.path_entries()
            .iter()
            .any(|entry| entry.as_os_str().is_empty() || entry.is_relative())
    }

    fn absolute(&self, path: &Path) -> Option<PathBuf> {
        if path.is_absolute() {
            Some(path.to_owned())
        } else {
            self.cwd.map(|cwd| cwd.join(path))
        }
    }

    /// The file `word` runs, as the shell would find it.
    fn resolve(&self, word: &str) -> Option<PathBuf> {
        if word.contains('/') {
            return self.absolute(Path::new(word)).filter(|path| path.is_file());
        }
        self.path_entries().into_iter().find_map(|entry| {
            let entry = if entry.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                entry
            };
            let candidate = self.absolute(&entry)?.join(word);
            let metadata = std::fs::metadata(&candidate).ok()?;
            (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then_some(candidate)
        })
    }

    /// The folded names `word` answers to, and the identity of its file.
    pub(crate) fn program(&self, word: &str) -> (Vec<String>, Option<(u64, u64)>) {
        let mut names = vec![file_name(word)];
        let mut identity = None;
        if let Some(resolved) = self.resolve(word)
            && let Ok(canonical) = std::fs::canonicalize(&resolved)
        {
            let name = file_name(&canonical.to_string_lossy());
            if !names.contains(&name) {
                names.push(name);
            }
            identity = std::fs::metadata(&canonical)
                .ok()
                .map(|metadata| (metadata.dev(), metadata.ino()));
        }
        (names, identity)
    }
}

/// The folded last path component of `word`.
fn file_name(word: &str) -> String {
    fold_case(word.rsplit('/').next().unwrap_or(word))
}

fn named(names: &[String], list: &[&str]) -> bool {
    names.iter().any(|name| list.contains(&name.as_str()))
}

/// Strip transparent wrappers from `argv`. `None` if a wrapper is used in a
/// way this reader does not follow.
fn strip_wrappers(mut argv: Vec<String>) -> Option<Vec<String>> {
    loop {
        let Some(first) = argv.first() else {
            return Some(argv);
        };
        let rest = &argv[1..];
        let consumed = match file_name(first).as_str() {
            "command" => match rest.first().map(String::as_str) {
                // `command -v rm` only reports where `rm` is.
                Some("-v" | "-V") => return Some(argv),
                Some("-p") => match rest.get(1).map(String::as_str) {
                    Some("--") => 3,
                    _ => 2,
                },
                Some("--") => 2,
                Some(flag) if flag.starts_with('-') => return None,
                _ => 1,
            },
            "exec" => match rest.first() {
                Some(flag) if flag.starts_with('-') && flag != "--" => return None,
                Some(flag) if flag == "--" => 2,
                _ => 1,
            },
            "nohup" => match rest.first().map(String::as_str) {
                Some("--") => 2,
                Some(flag) if flag.starts_with('-') => return None,
                _ => 1,
            },
            "time" => {
                let mut count = 1;
                for word in rest {
                    match word.as_str() {
                        "-p" => count += 1,
                        "--" => {
                            count += 1;
                            break;
                        }
                        flag if flag.starts_with('-') => return None,
                        _ => break,
                    }
                }
                count
            }
            "env" => {
                let mut count = 1;
                let mut words = rest.iter();
                while let Some(word) = words.next() {
                    match word.as_str() {
                        "-i" | "--ignore-environment" | "-" => count += 1,
                        "-u" => {
                            words.next()?;
                            count += 2;
                        }
                        "--" => {
                            count += 1;
                            break;
                        }
                        flag if flag.starts_with("--unset=") => count += 1,
                        flag if flag.starts_with('-') => return None,
                        assignment if assignment.contains('=') => count += 1,
                        _ => break,
                    }
                }
                count
            }
            _ => return Some(argv),
        };
        argv.drain(..consumed.min(argv.len()));
    }
}

/// Whether an interpreter's arguments are a script file rather than inline
/// code or standard input.
fn runs_a_script(argv: &[String], lookup: &Lookup) -> bool {
    argv.get(1).is_some_and(|first| {
        !first.starts_with('-')
            && lookup
                .absolute(Path::new(first))
                .is_some_and(|path| path.is_file())
    })
}

/// Whether `awk` takes its program inline (no `-f`).
fn awk_inline(argv: &[String]) -> bool {
    !argv[1..]
        .iter()
        .any(|word| word == "-f" || (word.starts_with("-f") && word.len() > 2))
}

/// Whether `sed` is given its `e` command or the `s///e` flag, which run a
/// command.
fn sed_executes(argv: &[String], lookup: &Lookup) -> bool {
    argv[1..].iter().any(|word| {
        if word.starts_with('-') && !word.starts_with("-e") {
            return false;
        }
        if lookup
            .absolute(Path::new(word))
            .is_some_and(|path| path.is_file())
        {
            return false;
        }
        word.split([';', '\n']).any(sed_command_executes)
    })
}

fn sed_command_executes(command: &str) -> bool {
    let command = command
        .trim_start()
        .trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '$' | ',' | '!' | ' '));
    let mut chars = command.chars();
    match chars.next() {
        Some('e') => true,
        Some('s') => {
            let Some(delimiter) = chars.next() else {
                return false;
            };
            // Skip the pattern and the replacement, then read the flags.
            let mut seen = 0;
            let mut escaped = false;
            for c in chars.by_ref() {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == delimiter {
                    seen += 1;
                    if seen == 2 {
                        break;
                    }
                }
            }
            seen == 2
                && chars
                    .take_while(|c| !c.is_whitespace() && *c != '}')
                    .any(|c| c == 'e')
        }
        _ => false,
    }
}

/// The programs `command` would run, or `None` if it cannot be checked
/// against a denial reliably. See the module documentation.
pub(crate) fn analyze(command: &str, lookup: &Lookup) -> Option<Vec<Invocation>> {
    let segments = segments(command)?;
    let compound = segments.len() > 1;
    let last = segments.len().saturating_sub(1);
    let relative_path = lookup.has_relative_entry();
    let mut invocations = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        let words: Vec<String> = segment.words.iter().map(|word| word.text.clone()).collect();
        if words.first().is_some_and(|word| !plain_command_word(word)) {
            return None;
        }
        let argv = strip_wrappers(words)?;
        let binds_files = segment.writes_file;
        let Some(first) = argv.first() else {
            if compound && binds_files && index != last {
                return None;
            }
            continue;
        };
        if !plain_command_word(first) {
            return None;
        }
        let (names, identity) = lookup.program(first);
        if named(&names, OPAQUE) || named(&names, SHELLS) {
            return None;
        }
        let interpreter = names.iter().any(|name| {
            INTERPRETERS
                .iter()
                .any(|interpreter| name.starts_with(interpreter))
        });
        if interpreter && !runs_a_script(&argv, lookup) {
            return None;
        }
        let awk = named(&names, &["awk", "gawk", "mawk", "nawk"]);
        if awk && awk_inline(&argv) {
            return None;
        }
        if named(&names, &["sed", "gsed"]) && sed_executes(&argv, lookup) {
            return None;
        }
        if named(&names, &["find", "gfind"])
            && argv[1..].iter().any(|word| {
                matches!(
                    word.as_str(),
                    "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete"
                )
            })
        {
            return None;
        }
        if named(&names, &["git"]) && argv[1..].iter().any(|word| word == "-c") {
            return None;
        }
        if compound {
            // A later part may run a file an earlier part created or changed.
            if index > 0 && first.contains('/') {
                return None;
            }
            let binds = binds_files
                || named(&names, BINDERS)
                || (relative_path && named(&names, DIRECTORY_CHANGERS));
            if binds && index != last {
                return None;
            }
        }
        invocations.push(Invocation {
            argv,
            names,
            identity,
        });
    }
    Some(invocations)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(segments: &[Segment]) -> Vec<Vec<&str>> {
        segments
            .iter()
            .map(|segment| {
                segment
                    .words
                    .iter()
                    .map(|word| word.text.as_str())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn segments_follow_quoting_operators_and_redirections() {
        let parsed = segments("ls -la 'a;b' | wc -l && echo \"x|y\" > out 2>&1; true").unwrap();
        assert_eq!(
            texts(&parsed),
            [
                vec!["ls", "-la", "a;b"],
                vec!["wc", "-l"],
                vec!["echo", "x|y"],
                vec!["true"],
            ]
        );
        assert!(parsed[2].writes_file && !parsed[0].writes_file);
        assert_eq!(texts(&segments("make 2> log").unwrap()), [vec!["make"]]);
        assert_eq!(texts(&segments("a &> log").unwrap()), [vec!["a"]]);
        assert_eq!(texts(&segments("rm x # rm y").unwrap()), [vec!["rm", "x"]]);
        assert_eq!(
            texts(&segments("! grep x f").unwrap()),
            [vec!["grep", "x", "f"]]
        );
        assert!(segments("ls docs/*").unwrap().first().unwrap().words[1].expands);
        assert!(!segments("ls 'docs/*'").unwrap()[0].words[1].expands);
        for unreadable in [
            "echo $HOME",
            "cat <<EOF",
            "echo 'open",
            "echo \"open",
            "ls >",
            "case x in a) ;; esac",
            "echo \\",
        ] {
            assert!(segments(unreadable).is_none(), "{unreadable}");
        }
    }

    #[test]
    fn wrappers_are_stripped_or_refused() {
        let strip = |text: &str| {
            strip_wrappers(text.split(' ').map(str::to_owned).collect()).map(|argv| argv.join(" "))
        };
        assert_eq!(strip("command rm -rf x").as_deref(), Some("rm -rf x"));
        assert_eq!(strip("exec nohup time -p rm x").as_deref(), Some("rm x"));
        assert_eq!(strip("env -i A=1 -u B rm x").as_deref(), Some("rm x"));
        assert_eq!(strip("command -v rm").as_deref(), Some("command -v rm"));
        assert_eq!(strip("env").as_deref(), Some(""));
        assert!(strip("exec -a name rm").is_none());
        assert!(strip("env -S rm").is_none());
        assert!(strip("time -l rm").is_none());
    }

    #[test]
    fn sed_and_awk_that_run_commands_are_recognised() {
        let lookup = Lookup {
            cwd: None,
            search_path: Some(OsStr::new("")),
        };
        let argv = |text: &str| -> Vec<String> { shell_words::split(text).unwrap() };
        assert!(sed_executes(&argv("sed 's/x/rm -rf y/e' f"), &lookup));
        assert!(sed_executes(&argv("sed -e '1e rm -rf y' f"), &lookup));
        assert!(sed_executes(&argv("sed 's|a|b|ge'"), &lookup));
        assert!(!sed_executes(&argv("sed 's/e/E/g' file"), &lookup));
        assert!(!sed_executes(&argv("sed -n '/error/p' log"), &lookup));
        assert!(awk_inline(&argv("awk '{system(\"rm x\")}'")));
        assert!(!awk_inline(&argv("awk -f prog.awk data")));
    }
}

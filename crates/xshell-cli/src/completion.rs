use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};
use std::path::PathBuf;
use std::sync::Mutex;
use xshell_session::{SessionClient, ShellCompletionResult, complete_shell};

const CONTROL_COMMANDS: &[&str] = &[
    "//agent",
    "//audit",
    "//close",
    "//connect",
    "//detach",
    "//help",
    "//model",
    "//new",
    "//quit",
    "//sessions",
    "//status",
    "//switch",
    "//tools",
    "//view",
];

/// Sub-commands recognised after `//model`.
const MODEL_SUBCOMMANDS: &[&str] = &["list", "show", "use"];
const VIEWERS: &[&str] = &["markdown", "rst"];

pub struct XshellHelper {
    cwd: PathBuf,
    shell_completion_enabled: bool,
    remote_shell_completion: Option<RemoteShellCompletion>,
    model_profiles: Vec<String>,
    session_names: Vec<String>,
}

struct RemoteShellCompletion {
    client: Mutex<Option<SessionClient>>,
    session_id: String,
}

impl XshellHelper {
    pub fn new(cwd: PathBuf, model_profiles: Vec<String>) -> Self {
        Self {
            cwd,
            shell_completion_enabled: true,
            remote_shell_completion: None,
            model_profiles,
            session_names: Vec::new(),
        }
    }

    pub fn set_cwd(&mut self, cwd: PathBuf) {
        self.cwd = cwd;
    }

    pub fn set_shell_completion_enabled(&mut self, enabled: bool) {
        self.shell_completion_enabled = enabled;
    }

    pub fn set_remote_shell_completion(&mut self, remote: Option<(SessionClient, String)>) {
        self.remote_shell_completion = remote.map(|(client, session_id)| RemoteShellCompletion {
            client: Mutex::new(Some(client)),
            session_id,
        });
    }

    #[allow(dead_code)]
    pub fn set_model_profiles(&mut self, profiles: Vec<String>) {
        self.model_profiles = profiles;
    }

    pub fn set_session_names(&mut self, mut names: Vec<String>) {
        names.sort();
        names.dedup();
        self.session_names = names;
    }

    fn shell_candidates(&self, line: &str, pos: usize) -> Option<ShellCompletionResult> {
        if !self.shell_completion_enabled {
            return None;
        }
        if let Some(remote) = &self.remote_shell_completion {
            let Ok(mut guard) = remote.client.lock() else {
                return None;
            };
            let client = guard.as_mut()?;
            return match client.complete_shell(remote.session_id.clone(), line.into(), pos) {
                Ok(result) => Some(result),
                Err(_) => {
                    *guard = None;
                    None
                }
            };
        }
        complete_shell(line, pos, &self.cwd).ok()
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

            if let Some(result) = complete_view(self, line, pos) {
                return Ok(result);
            }

            // Model profile completion: "//model <prefix>" or "//model use <prefix>"
            if let Some(result) = complete_model(line, pos, &self.model_profiles) {
                return Ok(result);
            }

            if let Some(result) =
                complete_single_argument(line, pos, "//switch", &self.session_names)
            {
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
        let Some(result) = self.shell_candidates(line, pos) else {
            return Ok((pos, Vec::new()));
        };
        Ok((
            result.start,
            result
                .candidates
                .into_iter()
                .map(|candidate| Pair {
                    display: candidate.display,
                    replacement: candidate.replacement,
                })
                .collect(),
        ))
    }
}

fn complete_view(helper: &XshellHelper, line: &str, pos: usize) -> Option<(usize, Vec<Pair>)> {
    let prefix = &line[..pos];
    let rest = prefix.strip_prefix("//view")?;
    if rest.is_empty() {
        let synthetic = "$cat ";
        let result = helper.shell_candidates(synthetic, synthetic.len())?;
        return Some((
            "//view".len(),
            result
                .candidates
                .into_iter()
                .map(|candidate| Pair {
                    display: candidate.display,
                    replacement: format!(" {}", candidate.replacement),
                })
                .collect(),
        ));
    }
    if !rest.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    let arguments = rest.trim_start_matches(char::is_whitespace);
    if let Some(viewer_prefix) = arguments.strip_prefix("--as=")
        && !viewer_prefix.contains(char::is_whitespace)
    {
        let start = prefix.len() - viewer_prefix.len() - "--as=".len();
        let candidates = VIEWERS
            .iter()
            .filter(|viewer| viewer.starts_with(viewer_prefix))
            .map(|viewer| Pair {
                display: (*viewer).into(),
                replacement: format!("--as={viewer}"),
            })
            .collect();
        return Some((start, candidates));
    }
    let words = arguments.split_whitespace().collect::<Vec<_>>();
    let ends_with_space = arguments.chars().last().is_some_and(char::is_whitespace);
    if words.first() == Some(&"--as")
        && ((words.len() == 1 && ends_with_space) || (words.len() == 2 && !ends_with_space))
    {
        let viewer_prefix = words.get(1).copied().unwrap_or("");
        let start = prefix.len() - viewer_prefix.len();
        let candidates = VIEWERS
            .iter()
            .filter(|viewer| viewer.starts_with(viewer_prefix))
            .map(|viewer| Pair {
                display: (*viewer).into(),
                replacement: (*viewer).into(),
            })
            .collect();
        return Some((start, candidates));
    }
    if arguments.trim_end().ends_with("--as") {
        return Some((pos, Vec::new()));
    }

    let fragment_start = prefix
        .rfind(char::is_whitespace)
        .map_or("//view".len(), |index| index + 1);
    let fragment = &prefix[fragment_start..];
    if fragment.starts_with('-') {
        return Some((pos, Vec::new()));
    }
    let synthetic = format!("$cat {fragment}");
    let result = helper.shell_candidates(&synthetic, synthetic.len())?;
    Some((
        fragment_start,
        result
            .candidates
            .into_iter()
            .map(|candidate| Pair {
                display: candidate.display,
                replacement: candidate.replacement,
            })
            .collect(),
    ))
}

fn complete_single_argument(
    line: &str,
    pos: usize,
    command: &str,
    values: &[String],
) -> Option<(usize, Vec<Pair>)> {
    let prefix = &line[..pos];
    let rest = prefix.strip_prefix(command)?;
    if rest.is_empty() {
        return Some((command.len(), value_candidates("", values, " ")));
    }
    if !rest.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    let argument = rest.trim_start_matches(char::is_whitespace);
    if argument.contains(char::is_whitespace) {
        return Some((pos, Vec::new()));
    }
    let start = prefix.len() - argument.len();
    Some((start, value_candidates(argument, values, "")))
}

fn value_candidates(prefix: &str, values: &[String], replacement_prefix: &str) -> Vec<Pair> {
    values
        .iter()
        .filter(|value| value.starts_with(prefix))
        .map(|value| Pair {
            display: value.clone(),
            replacement: format!("{replacement_prefix}{value}"),
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use rustyline::history::DefaultHistory;
    use std::path::Path;

    #[test]
    fn path_completion_uses_xshell_cwd() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let result = complete_shell("$cat Cargo", 10, root).unwrap();
        assert!(
            result
                .candidates
                .iter()
                .any(|candidate| candidate.replacement == "Cargo.toml")
        );
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

    #[test]
    fn switch_completion_uses_session_catalog_and_inserts_space() {
        let mut helper = helper_with_profiles();
        helper.set_session_names(vec!["robot".into(), "bees".into(), "local:default".into()]);
        let history = DefaultHistory::new();
        let context = Context::new(&history);

        let line = "//switch";
        let (start, matches) = helper.complete(line, line.len(), &context).unwrap();
        let bees = matches
            .iter()
            .find(|candidate| candidate.display == "bees")
            .unwrap();
        assert_eq!(
            apply_completion(line, line.len(), start, &bees.replacement),
            "//switch bees"
        );

        let line = "//switch ro";
        let (start, matches) = helper.complete(line, line.len(), &context).unwrap();
        assert_eq!(start, 9);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].replacement, "robot");

        let line = "//switch local:d";
        let (start, matches) = helper.complete(line, line.len(), &context).unwrap();
        assert_eq!(start, 9);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].replacement, "local:default");
    }

    #[test]
    fn disabled_shell_completion_does_not_offer_local_candidates() {
        let mut helper = helper_with_profiles();
        helper.set_shell_completion_enabled(false);
        let history = DefaultHistory::new();
        let context = Context::new(&history);
        let line = "$gi";
        let (_, matches) = helper.complete(line, line.len(), &context).unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn completes_viewer_names_and_view_paths() {
        let helper = helper_with_profiles();
        let history = DefaultHistory::new();
        let context = Context::new(&history);

        let line = "//view --as r";
        let (start, matches) = helper.complete(line, line.len(), &context).unwrap();
        assert_eq!(start, 12);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].replacement, "rst");

        let line = "//view Cargo";
        let (start, matches) = helper.complete(line, line.len(), &context).unwrap();
        assert_eq!(start, 7);
        assert!(matches.iter().any(|pair| pair.replacement == "Cargo.toml"));

        let line = "//view";
        let (start, matches) = helper.complete(line, line.len(), &context).unwrap();
        assert_eq!(start, 6);
        assert!(matches.iter().any(|pair| pair.replacement == " Cargo.toml"));
    }
}

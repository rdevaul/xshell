//! Reading a deliberate answer at a prompt that authorizes an effect (F20).
//!
//! Keys typed while the agent works ("why", "okay", "sorry") used to answer
//! the next approval prompt at their first `y`. Every prompt that authorizes an
//! effect now follows the same rules:
//!
//! - pending terminal input is discarded before the prompt is drawn, and again
//!   after a short settling window, so typeahead and keys in flight never count;
//! - approval needs `y` (or `yes`) followed by Enter, with nothing else
//!   buffered behind the Enter;
//! - denial (`n`, or Enter alone, the documented `N` default) and abort (`q`)
//!   stay single keys, because answering "no" by accident is safe;
//! - any other byte discards the rest of the pending input and re-prompts; it is
//!   never skipped toward a later `y`.

use anyhow::Result;
use std::io::{self, IsTerminal};
use std::os::fd::AsRawFd;
use std::time::Duration;
use xshell_execution::ApprovalDecision;

/// How long after a prompt is drawn input is still treated as typeahead.
/// Long enough to cover a key already on its way when the prompt appeared,
/// short enough not to be felt.
pub(crate) const APPROVAL_SETTLE: Duration = Duration::from_millis(250);

const BACKSPACE: u8 = 0x08;
const DELETE: u8 = 0x7f;
const CTRL_C: u8 = 0x03;
const CTRL_D: u8 = 0x04;

/// What a chunk of raw keyboard input did to the answer being typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyOutcome {
    /// No decision yet; `typed()` holds the partial answer to echo.
    Pending,
    Decided(ApprovalDecision),
    /// The input was not an answer. Discard pending input and ask again.
    Reprompt,
}

/// The pure decision logic of a raw-mode approval prompt. It holds only a
/// prefix of `yes`; anything that cannot become one is rejected outright.
#[derive(Debug, Default)]
pub(crate) struct ApprovalKeys {
    typed: Vec<u8>,
}

impl ApprovalKeys {
    /// The partial answer typed so far (a prefix of `yes`).
    pub(crate) fn typed(&self) -> &[u8] {
        &self.typed
    }

    /// Forget a partial answer, as after a re-prompt.
    pub(crate) fn clear(&mut self) {
        self.typed.clear();
    }

    /// Feed one `read` worth of bytes. A chunk is what the terminal had
    /// buffered at once, so an approving Enter must be its last byte: `y\r`
    /// with more behind it is pasted or typed-ahead text, not an answer.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> KeyOutcome {
        for (index, &byte) in chunk.iter().enumerate() {
            let rest = &chunk[index + 1..];
            match byte {
                // Editing the pending answer is harmless.
                BACKSPACE | DELETE => {
                    self.typed.pop();
                }
                CTRL_C | CTRL_D => return KeyOutcome::Decided(ApprovalDecision::AbortTurn),
                b'\r' | b'\n' => {
                    if self.typed.is_empty() {
                        // Enter alone takes the documented default, deny.
                        return KeyOutcome::Decided(ApprovalDecision::Deny);
                    }
                    // A CR LF pair is one Enter.
                    let trailing = if byte == b'\r' && rest.first() == Some(&b'\n') {
                        &rest[1..]
                    } else {
                        rest
                    };
                    let complete = matches!(self.typed.as_slice(), b"y" | b"yes");
                    self.typed.clear();
                    if complete && trailing.is_empty() {
                        return KeyOutcome::Decided(ApprovalDecision::Approve);
                    }
                    return KeyOutcome::Reprompt;
                }
                _ => {
                    let key = byte.to_ascii_lowercase();
                    if self.typed.is_empty() {
                        match key {
                            b'n' => return KeyOutcome::Decided(ApprovalDecision::Deny),
                            b'q' => return KeyOutcome::Decided(ApprovalDecision::AbortTurn),
                            _ => {}
                        }
                    }
                    self.typed.push(key);
                    if !b"yes".starts_with(&self.typed) {
                        self.typed.clear();
                        return KeyOutcome::Reprompt;
                    }
                }
            }
        }
        KeyOutcome::Pending
    }
}

/// Before a line-mode prompt: discard typed-ahead lines when stdin is a
/// terminal. Piped stdin (tests, scripts) is an intended answer stream and is
/// left alone.
pub(crate) fn discard_line_typeahead() -> Result<()> {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        xshell_pty::discard_terminal_input(stdin.as_raw_fd())?;
    }
    Ok(())
}

/// After a line-mode prompt is drawn: let keys already in flight land, then
/// discard them. Their echo may remain on the line, but they cannot answer.
pub(crate) fn settle_line_prompt() -> Result<()> {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        std::thread::sleep(APPROVAL_SETTLE);
        xshell_pty::discard_terminal_input(stdin.as_raw_fd())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(chunks: &[&[u8]]) -> KeyOutcome {
        let mut keys = ApprovalKeys::default();
        for chunk in chunks {
            let outcome = keys.feed(chunk);
            if outcome != KeyOutcome::Pending {
                return outcome;
            }
        }
        KeyOutcome::Pending
    }

    const APPROVE: KeyOutcome = KeyOutcome::Decided(ApprovalDecision::Approve);
    const DENY: KeyOutcome = KeyOutcome::Decided(ApprovalDecision::Deny);
    const ABORT: KeyOutcome = KeyOutcome::Decided(ApprovalDecision::AbortTurn);

    #[test]
    fn f20_words_containing_y_never_approve() {
        // The issue's examples: the first `y` used to approve.
        assert_eq!(answer(&[b"why"]), KeyOutcome::Reprompt);
        assert_eq!(answer(&[b"w", b"h", b"y", b"\r"]), KeyOutcome::Reprompt);
        assert_eq!(answer(&[b"okay"]), KeyOutcome::Reprompt);
        assert_eq!(answer(&[b"sorry\r"]), KeyOutcome::Reprompt);
        assert_eq!(answer(&[b"yeah\r"]), KeyOutcome::Reprompt);
        assert_eq!(answer(&[b"yy\r"]), KeyOutcome::Reprompt);
        assert_eq!(answer(&[b"ye\r"]), KeyOutcome::Reprompt);
        // Arrow keys and other escape sequences are not answers.
        assert_eq!(answer(&[b"\x1b[A"]), KeyOutcome::Reprompt);
    }

    #[test]
    fn f20_a_lone_y_needs_enter() {
        assert_eq!(answer(&[b"y"]), KeyOutcome::Pending);
        assert_eq!(answer(&[b"Y"]), KeyOutcome::Pending);
        assert_eq!(answer(&[b"y", b"\r"]), APPROVE);
        assert_eq!(answer(&[b"y\r"]), APPROVE);
        assert_eq!(answer(&[b"Y", b"\n"]), APPROVE);
        assert_eq!(answer(&[b"y", b"\r\n"]), APPROVE);
        assert_eq!(answer(&[b"y", b"e", b"s", b"\r"]), APPROVE);
    }

    #[test]
    fn f20_input_buffered_behind_the_enter_does_not_approve() {
        assert_eq!(answer(&[b"y\rls"]), KeyOutcome::Reprompt);
        assert_eq!(answer(&[b"y", b"\r\r"]), KeyOutcome::Reprompt);
        assert_eq!(answer(&[b"y\r\ny\r\n"]), KeyOutcome::Reprompt);
    }

    #[test]
    fn f20_deny_and_abort_stay_single_keys() {
        assert_eq!(answer(&[b"n"]), DENY);
        assert_eq!(answer(&[b"N"]), DENY);
        assert_eq!(answer(&[b"no"]), DENY);
        // Enter alone is the documented default, deny, never approve.
        assert_eq!(answer(&[b"\r"]), DENY);
        assert_eq!(answer(&[b"\n"]), DENY);
        assert_eq!(answer(&[b"q"]), ABORT);
        assert_eq!(answer(&[b"\x03"]), ABORT);
        assert_eq!(answer(&[b"\x04"]), ABORT);
        // `n` after a started `y` is a typo, not a denial or an approval.
        assert_eq!(answer(&[b"yn"]), KeyOutcome::Reprompt);
    }

    #[test]
    fn f20_backspace_edits_the_pending_answer() {
        assert_eq!(answer(&[b"y", b"\x7f", b"\r"]), DENY);
        assert_eq!(answer(&[b"y", b"\x7f", b"n"]), DENY);
        assert_eq!(answer(&[b"ye", b"\x08", b"\r"]), APPROVE);
    }

    #[test]
    fn f20_a_reprompt_forgets_the_partial_answer() {
        let mut keys = ApprovalKeys::default();
        assert_eq!(keys.feed(b"y"), KeyOutcome::Pending);
        assert_eq!(keys.typed(), b"y");
        assert_eq!(keys.feed(b"x"), KeyOutcome::Reprompt);
        assert!(keys.typed().is_empty());
        assert_eq!(keys.feed(b"\r"), DENY);
    }
}

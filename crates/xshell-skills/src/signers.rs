//! The operator's trust files: who may sign skills, and which keys are
//! revoked.
//!
//! The trusted-signers file is a subset of OpenSSH's `allowed_signers`
//! format. Each line is
//!
//! ```text
//! principal [options] ssh-ed25519 BASE64 [comment]
//! ```
//!
//! Anything outside the subset is an error naming the line, never skipped:
//! a line the operator wrote and this reader ignored would be a signer they
//! believe is restricted, or trusted, and is not.

use std::collections::HashSet;
use std::path::Path;

use crate::SkillError;
use crate::sshsig::{PublicKey, decode_base64, fingerprint_of_blob};

const SIGNERS: &str = "trusted signers";
const REVOKED: &str = "revoked keys";

#[derive(Debug)]
struct Signer {
    principal: String,
    key: PublicKey,
    /// `None`: any namespace.
    namespaces: Option<Vec<String>>,
    /// Seconds since the epoch; the key is not valid before this.
    valid_after: Option<u64>,
    /// Seconds since the epoch; the key is not valid after this.
    valid_before: Option<u64>,
}

/// The signers an operator trusts.
#[derive(Debug, Default)]
pub struct TrustedSigners(Vec<Signer>);

impl TrustedSigners {
    pub fn load(path: &Path) -> Result<Self, SkillError> {
        Self::parse(&read_text(path)?)
    }

    pub fn parse(text: &str) -> Result<Self, SkillError> {
        let mut signers = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let fail = |reason: String| SkillError::TrustFile {
                file: SIGNERS,
                line: index + 1,
                reason,
            };
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            signers.push(parse_signer(line).map_err(fail)?);
        }
        Ok(Self(signers))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The principal whose key this is, if the key may sign in `namespace`
    /// at time `now` (seconds since the epoch). When several lines hold the
    /// key, the first one that allows the signature names the signer.
    pub(crate) fn signer_for(
        &self,
        key: &PublicKey,
        namespace: &str,
        now: u64,
    ) -> Result<&str, SkillError> {
        let mut refusal = None;
        for signer in self.0.iter().filter(|signer| signer.key == *key) {
            match signer.allows(namespace, now) {
                Ok(()) => return Ok(&signer.principal),
                Err(reason) => {
                    refusal.get_or_insert(SkillError::SignerNotValid {
                        principal: signer.principal.clone(),
                        reason,
                    });
                }
            }
        }
        Err(refusal.unwrap_or_else(|| SkillError::UntrustedKey {
            fingerprint: key.fingerprint(),
        }))
    }
}

impl Signer {
    fn allows(&self, namespace: &str, now: u64) -> Result<(), String> {
        if let Some(namespaces) = &self.namespaces
            && !namespaces.iter().any(|allowed| allowed == namespace)
        {
            return Err(format!(
                "the key is not trusted for namespace {namespace:?}"
            ));
        }
        if self.valid_after.is_some_and(|after| now < after) {
            return Err("the key is not valid yet (valid-after)".into());
        }
        if self.valid_before.is_some_and(|before| now > before) {
            return Err("the key is no longer valid (valid-before)".into());
        }
        Ok(())
    }
}

fn parse_signer(line: &str) -> Result<Signer, String> {
    let tokens = split_tokens(line)?;
    let mut tokens = tokens.iter().map(String::as_str);
    let principal = tokens.next().unwrap_or_default();
    if principal.contains(['*', '?', '!', ',', '"']) {
        return Err(format!(
            "principal {principal:?}: write one plain name per line \
             (patterns and lists are not supported)"
        ));
    }

    let mut next = tokens
        .next()
        .ok_or("expected a key type after the principal")?;
    let mut signer = Signer {
        principal: principal.to_owned(),
        key: PublicKey::from_bytes([0; 32]),
        namespaces: None,
        valid_after: None,
        valid_before: None,
    };
    if !is_key_type(next) {
        parse_options(next, &mut signer)?;
        next = tokens
            .next()
            .ok_or("expected a key type after the options")?;
    }
    let blob = tokens.next().ok_or("expected a key after the key type")?;
    let blob = decode_base64(blob).ok_or("the key is not base64")?;
    signer.key = PublicKey::from_blob(&blob).map_err(|error| match error {
        SkillError::Unsupported(what) => what,
        other => other.to_string(),
    })?;
    if next != "ssh-ed25519" {
        return Err(format!("key type {next} does not match the key"));
    }
    Ok(signer)
}

fn is_key_type(token: &str) -> bool {
    ["ssh-", "ecdsa-", "sk-", "rsa-"]
        .iter()
        .any(|prefix| token.starts_with(prefix))
}

fn parse_options(options: &str, signer: &mut Signer) -> Result<(), String> {
    for option in split_options(options)? {
        let (name, value) = match option.split_once('=') {
            Some((name, value)) => (name, Some(unquote(value)?)),
            None => (option.as_str(), None),
        };
        match (name.to_ascii_lowercase().as_str(), value) {
            ("cert-authority", _) => {
                return Err("certificate authorities are not supported".into());
            }
            ("namespaces", Some(value)) => {
                let namespaces: Vec<String> = value.split(',').map(str::to_owned).collect();
                if namespaces
                    .iter()
                    .any(|n| n.is_empty() || n.contains(['*', '?', '!']))
                {
                    return Err(format!(
                        "namespaces={value:?}: list exact names (patterns are not supported)"
                    ));
                }
                signer.namespaces = Some(namespaces);
            }
            ("valid-after", Some(value)) => signer.valid_after = Some(parse_time(value)?),
            ("valid-before", Some(value)) => signer.valid_before = Some(parse_time(value)?),
            _ => return Err(format!("option {option:?} is not supported")),
        }
    }
    Ok(())
}

/// Split a line on whitespace, keeping double-quoted parts together.
fn split_tokens(line: &str) -> Result<Vec<String>, String> {
    split_outside_quotes(line, char::is_whitespace)
}

/// Split an options field on commas, keeping double-quoted parts together.
fn split_options(options: &str) -> Result<Vec<String>, String> {
    split_outside_quotes(options, |c| c == ',')
}

fn split_outside_quotes(
    text: &str,
    is_separator: impl Fn(char) -> bool,
) -> Result<Vec<String>, String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in text.chars() {
        if c == '"' {
            quoted = !quoted;
        }
        if !quoted && is_separator(c) {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if quoted {
        return Err("a double quote is not closed".into());
    }
    if !current.is_empty() {
        parts.push(current);
    }
    Ok(parts)
}

fn unquote(value: &str) -> Result<&str, String> {
    let inner = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(value);
    if inner.contains('"') {
        return Err(format!("misplaced double quote in {value:?}"));
    }
    Ok(inner)
}

/// `YYYYMMDD`, `YYYYMMDDHHMM` or `YYYYMMDDHHMMSS`, followed by `Z`.
///
/// OpenSSH reads a time without the `Z` in the machine's local time zone. A
/// daemon's time zone is not something an operator should have to reason
/// about when writing a trust file, so only UTC times are accepted.
fn parse_time(value: &str) -> Result<u64, String> {
    let bad = || format!("time {value:?}: use YYYYMMDD[HHMM[SS]]Z");
    let digits = value.strip_suffix('Z').ok_or_else(|| {
        format!("time {value:?} has no time zone: write it in UTC with a trailing Z")
    })?;
    if !matches!(digits.len(), 8 | 12 | 14) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let field = |range: std::ops::Range<usize>| -> u64 {
        digits
            .get(range)
            .map_or(0, |part| part.parse().unwrap_or(0))
    };
    let (year, month, day) = (field(0..4), field(4..6), field(6..8));
    let (hour, minute, second) = (field(8..10), field(10..12), field(12..14));
    let month_ok = (1..=12).contains(&month);
    if year < 1970 || !month_ok || day == 0 || day > days_in_month(year, month) {
        return Err(bad());
    }
    if hour > 23 || minute > 59 || second > 59 {
        return Err(bad());
    }
    let days = days_before_year(year)
        + (1..month).map(|m| days_in_month(year, m)).sum::<u64>()
        + (day - 1);
    Ok(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn is_leap(year: u64) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to January 1 of `year`.
fn days_before_year(year: u64) -> u64 {
    let leaps_through = |y: u64| y / 4 - y / 100 + y / 400;
    (year - 1970) * 365 + leaps_through(year - 1) - leaps_through(1969)
}

/// Keys that verify nothing.
///
/// One entry per line: a public key (`ssh-ed25519 BASE64 [comment]`, any key
/// type) or a fingerprint (`SHA256:…`). OpenSSH's binary revocation lists are
/// refused.
#[derive(Debug, Default)]
pub struct RevokedKeys(HashSet<String>);

impl RevokedKeys {
    pub fn load(path: &Path) -> Result<Self, SkillError> {
        let bytes = std::fs::read(path)
            .map_err(|error| SkillError::Io(format!("cannot read {}: {error}", path.display())))?;
        if bytes.starts_with(b"SSHKRL\n\0") {
            return Err(SkillError::Unsupported(format!(
                "{} is a binary key revocation list; list public keys or \
                 SHA256 fingerprints, one per line",
                path.display()
            )));
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| SkillError::Io(format!("{} is not text", path.display())))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, SkillError> {
        let mut revoked = HashSet::new();
        for (index, line) in text.lines().enumerate() {
            let fail = |reason: &str| SkillError::TrustFile {
                file: REVOKED,
                line: index + 1,
                reason: reason.to_owned(),
            };
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut tokens = line.split_whitespace();
            let first = tokens.next().unwrap_or_default();
            if let Some(digest) = first.strip_prefix("SHA256:") {
                // 32 bytes in unpadded base64.
                let valid = digest.len() == 43
                    && decode_base64(&format!("{digest}=")).is_some_and(|d| d.len() == 32);
                if !valid {
                    return Err(fail("not a SHA256 fingerprint"));
                }
                revoked.insert(first.to_owned());
            } else if is_key_type(first) {
                let blob = tokens
                    .next()
                    .and_then(decode_base64)
                    .ok_or_else(|| fail("expected a base64 key after the key type"))?;
                revoked.insert(fingerprint_of_blob(&blob));
            } else {
                return Err(fail("expected a public key or a SHA256: fingerprint"));
            }
        }
        Ok(Self(revoked))
    }

    pub fn contains(&self, key: &PublicKey) -> bool {
        self.0.contains(&key.fingerprint())
    }
}

fn read_text(path: &Path) -> Result<String, SkillError> {
    std::fs::read_to_string(path)
        .map_err(|error| SkillError::Io(format!("cannot read {}: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_utc_seconds() {
        assert_eq!(parse_time("19700101Z").unwrap(), 0);
        assert_eq!(parse_time("20000301Z").unwrap(), 951_868_800);
        assert_eq!(parse_time("202611270930Z").unwrap(), 1_795_771_800);
        assert_eq!(parse_time("20240229235959Z").unwrap(), 1_709_251_199);
    }

    #[test]
    fn bad_times_are_refused() {
        for value in [
            "20260101",
            "2026Z",
            "20261301Z",
            "20250229Z",
            "202601012460Z",
            "x",
        ] {
            assert!(parse_time(value).is_err(), "{value}");
        }
        assert!(parse_time("20260101").unwrap_err().contains("trailing Z"));
    }
}

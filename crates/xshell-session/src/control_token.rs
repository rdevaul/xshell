//! The control token that authenticates connections to `xshelld` (F27).
//!
//! The socket's owner check only says a peer runs as the same user. Agent
//! `run_shell` children, `$` commands and terminal jobs run as that user too,
//! so without more they could open sessions, answer pending approvals or
//! submit commands no approval policy covers. Each daemon start therefore
//! writes a fresh random token beside its socket, readable only by the user,
//! and every connection that controls a session must present it. The file
//! sits in the daemon's state directory by default, which agent read tools
//! may never read (F24); the daemon also refuses connections from its own
//! descendants outright, which covers a shell command that reads the file.

use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Length of a token in hex characters (two v4 UUIDs, 244 random bits).
const TOKEN_HEX_LEN: usize = 64;

/// Where the daemon serving `socket` keeps its control token: beside the
/// socket, as `<socket>.token`. Clients know the socket path, so they can
/// always find it.
pub fn control_token_path(socket: &Path) -> PathBuf {
    let mut name = socket.file_name().unwrap_or_default().to_os_string();
    name.push(".token");
    socket.with_file_name(name)
}

/// Create a new random token and write it to `path`, replacing any earlier
/// one, readable only by this user. Returns the token.
pub fn write_new_token(path: &Path) -> Result<String> {
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let temporary = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let written = (|| -> Result<()> {
        let _ = fs::remove_file(&temporary);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("cannot write {}", path.display()));
    }
    Ok(token)
}

/// Read the token at `path`. Refuses a symlink, a file another user owns,
/// or one others may read, since any of those would mean the token is not
/// the daemon's secret.
pub fn read_token(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| {
            format!(
                "cannot read the session service's control token {}; is xshelld running for \
this user?",
                path.display()
            )
        })?;
    let metadata = file.metadata()?;
    if metadata.uid() != xshell_platform::effective_uid() {
        bail!("{} is owned by another user", path.display());
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        bail!("{} is readable by other users", path.display());
    }
    let mut token = String::new();
    Read::by_ref(&mut file)
        .take(TOKEN_HEX_LEN as u64 + 1)
        .read_to_string(&mut token)?;
    let token = token.trim().to_owned();
    if token.len() != TOKEN_HEX_LEN || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("{} does not hold a valid control token", path.display());
    }
    Ok(token)
}

/// Compare two tokens in time that does not depend on where they differ.
pub fn tokens_match(presented: &str, expected: &str) -> bool {
    let (presented, expected) = (presented.as_bytes(), expected.as_bytes());
    if presented.len() != expected.len() {
        return false;
    }
    presented
        .iter()
        .zip(expected)
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn tokens_are_fresh_private_and_round_trip() {
        let temp = TempDir::new().unwrap();
        let path = control_token_path(&temp.path().join("xshelld.sock"));
        assert_eq!(path, temp.path().join("xshelld.sock.token"));
        let first = write_new_token(&path).unwrap();
        assert_eq!(read_token(&path).unwrap(), first);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let second = write_new_token(&path).unwrap();
        assert_ne!(first, second, "each start rotates the token");
        assert_eq!(read_token(&path).unwrap(), second);
    }

    #[test]
    fn a_token_others_can_read_is_refused() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("t.token");
        write_new_token(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_token(&path).is_err());
    }

    #[test]
    fn comparison_needs_an_exact_match() {
        let token = "a".repeat(TOKEN_HEX_LEN);
        assert!(tokens_match(&token, &token));
        assert!(!tokens_match(&"b".repeat(TOKEN_HEX_LEN), &token));
        assert!(!tokens_match("", &token));
        assert!(!tokens_match(&token[1..], &token));
    }
}

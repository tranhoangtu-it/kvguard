//! Owner-only files and directories, plus control-endpoint tokens.
//!
//! Traces, exports and the attach file contain prompts or credentials for the local control
//! API, so on Unix they are created `0600` (directories `0700`). On Windows the per-user
//! `%TEMP%` and default ACLs already restrict access to the owner.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use anyhow::{Context, Result};

/// Bytes of entropy in a control token (rendered as 32 hex characters).
const TOKEN_BYTES: usize = 16;

fn owner_only(options: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

/// Create (or truncate) a file readable only by its owner.
pub fn create_private(path: &Path) -> io::Result<File> {
    owner_only(OpenOptions::new().write(true).create(true).truncate(true)).open(path)
}

/// Open a file for appending, creating it owner-only when missing.
pub fn append_private(path: &Path) -> io::Result<File> {
    owner_only(OpenOptions::new().create(true).append(true)).open(path)
}

/// Create a directory (and parents) accessible only by its owner.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// A fresh random token for the control endpoints.
pub fn random_token() -> Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("operating system RNG unavailable")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Constant-time string comparison (no early exit on the first differing byte).
pub fn tokens_match(expected: &str, provided: &str) -> bool {
    let (a, b) = (expected.as_bytes(), provided.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_hex() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_eq!(a.len(), TOKEN_BYTES * 2);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn token_comparison() {
        assert!(tokens_match("abc123", "abc123"));
        assert!(!tokens_match("abc123", "abc124"));
        assert!(!tokens_match("abc123", "abc12"));
    }

    #[cfg(unix)]
    #[test]
    fn private_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("kvguard-test-{}", std::process::id()));
        create_private_dir(&dir).unwrap();
        let file = dir.join("secret.jsonl");
        create_private(&file).unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(dir);
    }
}

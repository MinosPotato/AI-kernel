//! The API key: how it is held, and how it is found.
//!
//! This is the first credential in the workspace, so the rules it establishes matter more
//! than the code that enforces them:
//!
//! * **A secret is never a configuration value.** The kernel's [`Config`](aik_core::Config) is a JSON tree that
//!   is cloned, merged, handed to every component and printed by `Debug`. Anything in it is
//!   effectively public to the process. So configuration names *where the key is* — an
//!   environment variable or a file — and never the key itself; see
//!   [`AnthropicSettings`](crate::settings::AnthropicSettings), which refuses a section that
//!   carries one anyway.
//! * **A secret does not implement `Display`, `Serialize` or a revealing `Debug`.** The only
//!   way to read it is a crate-private accessor, so a key cannot reach a
//!   log line, an audit record or an error message by accident — it takes a deliberate call
//!   in this crate.
//! * **Errors name the source, never the value.** Every failure here says which variable or
//!   file was consulted and what was wrong with the shape of what it held.

use std::fmt;
use std::path::Path;

use aik_core::{Error, Result};

/// The longest key this accepts, as a sanity bound rather than a protocol limit.
const MAX_KEY_BYTES: usize = 512;

/// An API key, held so that it cannot be printed, serialised or logged by accident.
///
/// Construction validates the shape: non-empty after trimming, visible ASCII throughout, and
/// bounded in length. The ASCII requirement is not cosmetic — the key becomes an HTTP header
/// value, and a newline or a control character in it is header injection.
pub struct ApiKey(String);

impl ApiKey {
    /// Validates and takes ownership of a raw key.
    ///
    /// `source` describes where it came from (a variable name, a path) and appears in any
    /// error. The key itself never does.
    pub fn new(raw: impl Into<String>, source: &str) -> Result<Self> {
        let raw = raw.into();
        let trimmed = raw.trim();

        if trimmed.is_empty() {
            return Err(Error::config(
                source.to_owned(),
                "the Anthropic API key is empty",
            ));
        }
        if trimmed.len() > MAX_KEY_BYTES {
            return Err(Error::config(
                source.to_owned(),
                format!("the Anthropic API key is longer than {MAX_KEY_BYTES} bytes"),
            ));
        }
        // Visible ASCII only. A key carrying a newline would otherwise be spliced into the
        // request as extra headers, and one carrying a NUL or a tab would be rejected far
        // from here, by the HTTP layer, with a message that does not say why.
        if !trimmed
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        {
            return Err(Error::config(
                source.to_owned(),
                "the Anthropic API key contains characters that cannot appear in an HTTP \
                 header; check for a stray newline or a shell-quoting mistake",
            ));
        }

        Ok(Self(trimmed.to_owned()))
    }

    /// The key itself.
    ///
    /// Crate-private on purpose: the only caller is the one place that builds the
    /// `x-api-key` header, and that header is marked sensitive so the HTTP stack will not
    /// log it either.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

impl Drop for ApiKey {
    /// Overwrites the bytes on the way out.
    ///
    /// Best effort, not a guarantee: a `String` that was reallocated during construction has
    /// already left a copy behind, and nothing here can reach it. It costs a memset and
    /// removes the *live* copy from a heap that a later core dump or a reused allocation
    /// could otherwise expose.
    fn drop(&mut self) {
        // Same length, so the replacement lands in the buffer that holds the key rather
        // than in a fresh one.
        let blanked = "\0".repeat(self.0.len());
        self.0.replace_range(.., &blanked);
    }
}

/// Where a key may be read from, in the order they are tried.
///
/// A file wins over a variable when both are configured, because naming a file is the more
/// deliberate act: a variable can be inherited from a parent process nobody inspected.
pub(crate) fn resolve(
    file: Option<&Path>,
    variable: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<ApiKey> {
    match file {
        Some(path) => from_file(path),
        None => match lookup(variable) {
            Some(value) => ApiKey::new(value, variable),
            None => Err(Error::config(
                variable.to_owned(),
                format!(
                    "no Anthropic API key: set `{variable}` in the environment, or point \
                     `api_key_file` at a file containing one"
                ),
            )),
        },
    }
}

/// Reads a key out of a file, refusing one that other users can read.
///
/// The permission check is the point of supporting a file at all: an environment variable is
/// visible to anything that can read `/proc/<pid>/environ` for this user, and a
/// world-readable file is worse rather than better. Refusing is fail-closed — a deployment
/// that meant to protect the key learns at startup that it did not, instead of running for
/// months believing it had.
fn from_file(path: &Path) -> Result<ApiKey> {
    let source = path.display().to_string();

    let metadata = std::fs::metadata(path).map_err(|error| {
        Error::config(
            source.clone(),
            format!("cannot read the Anthropic API key file: {error}"),
        )
    })?;
    if !metadata.is_file() {
        return Err(Error::config(
            source,
            "the Anthropic API key path is not a regular file",
        ));
    }
    check_permissions(&metadata, &source)?;

    let contents = std::fs::read_to_string(path).map_err(|error| {
        Error::config(
            source.clone(),
            format!("cannot read the Anthropic API key file: {error}"),
        )
    })?;
    ApiKey::new(contents, &source)
}

#[cfg(unix)]
fn check_permissions(metadata: &std::fs::Metadata, source: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(Error::config(
            source.to_owned(),
            format!(
                "the Anthropic API key file is readable by other users (mode {mode:04o}); \
                 run `chmod 600` on it"
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_metadata: &std::fs::Metadata, _source: &str) -> Result<()> {
    // No portable equivalent of the Unix mode bits. The file is still read; the deployment
    // is responsible for protecting it.
    Ok(())
}

/// The default place a key is looked for.
pub(crate) const DEFAULT_KEY_VARIABLE: &str = "ANTHROPIC_API_KEY";

/// Where the settings that name a key live, for error messages.
pub(crate) fn key_source_hint() -> String {
    format!("`api_key_env` (default `{DEFAULT_KEY_VARIABLE}`) or `api_key_file`")
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::ErrorKind;

    use std::path::PathBuf;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |name: &str| {
            owned
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn a_key_never_prints_itself() {
        let key = ApiKey::new("sk-ant-secret", "TEST").unwrap();
        assert_eq!(format!("{key:?}"), "ApiKey(<redacted>)");
        assert!(!format!("{key:?}").contains("secret"));
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        // The common case: a file written with a trailing newline.
        let key = ApiKey::new("  sk-ant-secret\n", "TEST").unwrap();
        assert_eq!(key.expose(), "sk-ant-secret");
    }

    #[test]
    fn an_embedded_newline_is_refused() {
        // Header injection: everything after the newline would become its own header.
        let error = ApiKey::new("sk-ant\r\nx-evil: 1", "TEST").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Config);
        assert!(!format!("{error}").contains("x-evil"), "{error}");
    }

    #[test]
    fn an_empty_or_blank_key_is_refused() {
        assert!(ApiKey::new("", "TEST").is_err());
        assert!(ApiKey::new("   \n", "TEST").is_err());
    }

    #[test]
    fn an_absurdly_long_key_is_refused() {
        assert!(ApiKey::new("k".repeat(MAX_KEY_BYTES + 1), "TEST").is_err());
    }

    #[test]
    fn errors_never_quote_the_key() {
        let error = ApiKey::new("sk-ant-\u{7f}-secret", "TEST").unwrap_err();
        assert!(!format!("{error}").contains("secret"), "{error}");
    }

    #[test]
    fn the_variable_is_read_when_no_file_is_configured() {
        let key = resolve(
            None,
            "ANTHROPIC_API_KEY",
            env(&[("ANTHROPIC_API_KEY", "sk-a")]),
        )
        .unwrap();
        assert_eq!(key.expose(), "sk-a");
    }

    #[test]
    fn an_absent_variable_names_itself_and_the_alternative() {
        let error = resolve(None, "ANTHROPIC_API_KEY", env(&[])).unwrap_err();
        let text = format!("{error}");
        assert!(text.contains("ANTHROPIC_API_KEY"), "{text}");
        assert!(text.contains("api_key_file"), "{text}");
    }

    #[test]
    fn a_file_wins_over_a_variable() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_key(directory.path(), "sk-from-file", 0o600);

        let key = resolve(
            Some(&path),
            "ANTHROPIC_API_KEY",
            env(&[("ANTHROPIC_API_KEY", "sk-from-env")]),
        )
        .unwrap();

        assert_eq!(key.expose(), "sk-from-file");
    }

    #[test]
    fn a_world_readable_key_file_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_key(directory.path(), "sk-exposed", 0o644);

        let error = resolve(Some(&path), "ANTHROPIC_API_KEY", env(&[])).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::Config);
        assert!(
            format!("{error}").contains("readable by other users"),
            "{error}"
        );
        assert!(!format!("{error}").contains("sk-exposed"), "{error}");
    }

    #[test]
    fn a_missing_key_file_names_the_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("absent");

        let error = resolve(Some(&path), "ANTHROPIC_API_KEY", env(&[])).unwrap_err();

        assert!(format!("{error}").contains("absent"), "{error}");
    }

    #[test]
    fn a_directory_is_not_a_key_file() {
        let directory = tempfile::tempdir().unwrap();
        let error = resolve(Some(directory.path()), "ANTHROPIC_API_KEY", env(&[])).unwrap_err();
        assert!(format!("{error}").contains("not a regular file"), "{error}");
    }

    fn write_key(directory: &Path, contents: &str, mode: u32) -> PathBuf {
        let path = directory.join("key");
        std::fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let _ = mode;
        path
    }
}

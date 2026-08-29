//! The API key: how it is held, and how it is found.
//!
//! The rules are the ones [`aik-anthropic`](https://docs.rs/aik-anthropic) established, and
//! they are restated rather than shared because the parts that differ are the parts a
//! deployment reads: the variable consulted, the wording of every refusal, and the one
//! relaxation this dialect needs and the Messages API does not — an endpoint that takes no
//! credential at all.
//!
//! * **A secret is never a configuration value.** The kernel's [`Config`](aik_core::Config)
//!   is a JSON tree that is cloned, merged, handed to every component and printed by
//!   `Debug`. So configuration names *where the key is* — an environment variable or a file
//!   — and never the key itself; see [`OpenAiSettings`](crate::settings::OpenAiSettings),
//!   which refuses a section that carries one anyway.
//! * **A secret does not implement `Display`, `Serialize` or a revealing `Debug`.** The only
//!   way to read it is a crate-private accessor, so a key cannot reach a log line, an audit
//!   record or an error message by accident.
//! * **Errors name the source, never the value.** Every failure here says which variable or
//!   file was consulted and what was wrong with the shape of what it held.

use std::fmt;
use std::path::Path;

use aik_core::{Error, Result};

/// The longest key this accepts, as a sanity bound rather than a protocol limit.
///
/// Larger than the Anthropic provider's bound because this dialect is spoken by gateways
/// whose "key" is a signed token rather than an opaque identifier, and a JWT with a few
/// claims in it runs past 512 bytes without being suspicious.
const MAX_KEY_BYTES: usize = 4096;

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
                "the OpenAI API key is empty",
            ));
        }
        if trimmed.len() > MAX_KEY_BYTES {
            return Err(Error::config(
                source.to_owned(),
                format!("the OpenAI API key is longer than {MAX_KEY_BYTES} bytes"),
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
                "the OpenAI API key contains characters that cannot appear in an HTTP header; \
                 check for a stray newline or a shell-quoting mistake",
            ));
        }

        Ok(Self(trimmed.to_owned()))
    }

    /// The key itself.
    ///
    /// Crate-private on purpose: the only caller is the one place that builds the
    /// `authorization` header, and that header is marked sensitive so the HTTP stack will
    /// not log it either.
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
///
/// `required` is what a deployment set in
/// [`api_key_required`](crate::settings::OpenAiSettings::api_key_required). When it is
/// `false` and nothing supplies a key, this returns `None` and the request goes out with no
/// `authorization` header — which is how a local inference server that has no notion of an
/// account is talked to. A key that *is* present is still validated and still sent, so
/// turning the requirement off does not turn the checks off.
pub(crate) fn resolve(
    file: Option<&Path>,
    variable: &str,
    required: bool,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<ApiKey>> {
    match file {
        Some(path) => from_file(path).map(Some),
        None => match lookup(variable) {
            Some(value) => ApiKey::new(value, variable).map(Some),
            None if !required => Ok(None),
            None => Err(Error::config(
                variable.to_owned(),
                format!(
                    "no OpenAI API key: set `{variable}` in the environment, point \
                     `api_key_file` at a file containing one, or set `api_key_required = \
                     false` for a loopback endpoint that takes no credential"
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
///
/// A configured file is always required to exist, whatever `api_key_required` says: naming a
/// path is a statement that the key is there, and falling back to no credential because the
/// file was missing would turn a typo into an unauthenticated request.
fn from_file(path: &Path) -> Result<ApiKey> {
    let source = path.display().to_string();

    let metadata = std::fs::metadata(path).map_err(|error| {
        Error::config(
            source.clone(),
            format!("cannot read the OpenAI API key file: {error}"),
        )
    })?;
    if !metadata.is_file() {
        return Err(Error::config(
            source,
            "the OpenAI API key path is not a regular file",
        ));
    }
    check_permissions(&metadata, &source)?;

    let contents = std::fs::read_to_string(path).map_err(|error| {
        Error::config(
            source.clone(),
            format!("cannot read the OpenAI API key file: {error}"),
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
                "the OpenAI API key file is readable by other users (mode {mode:04o}); run \
                 `chmod 600` on it"
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
pub(crate) const DEFAULT_KEY_VARIABLE: &str = "OPENAI_API_KEY";

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
        let key = ApiKey::new("sk-secret", "TEST").unwrap();
        assert_eq!(format!("{key:?}"), "ApiKey(<redacted>)");
        assert!(!format!("{key:?}").contains("secret"));
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        // The common case: a file written with a trailing newline.
        let key = ApiKey::new("  sk-secret\n", "TEST").unwrap();
        assert_eq!(key.expose(), "sk-secret");
    }

    #[test]
    fn an_embedded_newline_is_refused() {
        // Header injection: everything after the newline would become its own header.
        let error = ApiKey::new("sk-x\r\nx-evil: 1", "TEST").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Config);
        assert!(!format!("{error}").contains("x-evil"), "{error}");
    }

    #[test]
    fn an_empty_or_blank_key_is_refused() {
        assert!(ApiKey::new("", "TEST").is_err());
        assert!(ApiKey::new("   \n", "TEST").is_err());
    }

    #[test]
    fn a_long_token_is_accepted_but_an_absurd_one_is_not() {
        // A gateway's signed token is legitimately long; a megabyte of it is not a key.
        assert!(ApiKey::new("k".repeat(1024), "TEST").is_ok());
        assert!(ApiKey::new("k".repeat(MAX_KEY_BYTES + 1), "TEST").is_err());
    }

    #[test]
    fn errors_never_quote_the_key() {
        let error = ApiKey::new("sk-\u{7f}-secret", "TEST").unwrap_err();
        assert!(!format!("{error}").contains("secret"), "{error}");
    }

    #[test]
    fn the_variable_is_read_when_no_file_is_configured() {
        let key = resolve(
            None,
            "OPENAI_API_KEY",
            true,
            env(&[("OPENAI_API_KEY", "sk-a")]),
        )
        .unwrap()
        .expect("a key");
        assert_eq!(key.expose(), "sk-a");
    }

    #[test]
    fn an_absent_variable_names_itself_and_the_alternatives() {
        let error = resolve(None, "OPENAI_API_KEY", true, env(&[])).unwrap_err();
        let text = format!("{error}");
        assert!(text.contains("OPENAI_API_KEY"), "{text}");
        assert!(text.contains("api_key_file"), "{text}");
        assert!(text.contains("api_key_required"), "{text}");
    }

    #[test]
    fn an_endpoint_that_needs_no_credential_resolves_to_none() {
        assert!(
            resolve(None, "OPENAI_API_KEY", false, env(&[]))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_key_that_is_present_is_still_validated_when_none_is_required() {
        // Relaxing the requirement must not relax the checks on what is actually sent.
        let error = resolve(
            None,
            "OPENAI_API_KEY",
            false,
            env(&[("OPENAI_API_KEY", "sk-x\nx-evil: 1")]),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Config);
    }

    #[test]
    fn a_file_wins_over_a_variable() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_key(directory.path(), "sk-from-file", 0o600);

        let key = resolve(
            Some(&path),
            "OPENAI_API_KEY",
            true,
            env(&[("OPENAI_API_KEY", "sk-from-env")]),
        )
        .unwrap()
        .expect("a key");

        assert_eq!(key.expose(), "sk-from-file");
    }

    #[test]
    fn a_world_readable_key_file_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_key(directory.path(), "sk-exposed", 0o644);

        let error = resolve(Some(&path), "OPENAI_API_KEY", true, env(&[])).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::Config);
        assert!(
            format!("{error}").contains("readable by other users"),
            "{error}"
        );
        assert!(!format!("{error}").contains("sk-exposed"), "{error}");
    }

    #[test]
    fn a_missing_key_file_is_an_error_even_when_no_key_is_required() {
        // Naming a path is a statement that the key is there. Falling back to an
        // unauthenticated request because of a typo is the wrong direction to fail in.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("absent");

        let error = resolve(Some(&path), "OPENAI_API_KEY", false, env(&[])).unwrap_err();

        assert!(format!("{error}").contains("absent"), "{error}");
    }

    #[test]
    fn a_directory_is_not_a_key_file() {
        let directory = tempfile::tempdir().unwrap();
        let error = resolve(Some(directory.path()), "OPENAI_API_KEY", true, env(&[])).unwrap_err();
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

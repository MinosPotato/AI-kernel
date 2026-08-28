//! Configuration for the Anthropic provider.
//!
//! Everything here is a *location* or a *limit*. The one thing that is neither — the API key
//! — is deliberately absent: see [`credentials`](crate::credentials) for why, and
//! [`AnthropicSettings::read`] for what happens to a configuration that tries anyway.

use std::path::PathBuf;

use aik_core::{Config, Error, Result};
use serde::{Deserialize, Serialize};

fn default_endpoint() -> String {
    "https://api.anthropic.com".to_owned()
}

fn default_api_version() -> String {
    // The Messages API is versioned by a header rather than by the path, and the value is
    // a date. Pinned rather than tracked: a provider that silently followed the newest
    // version would change the shape of what it parses without anybody deploying anything.
    "2023-06-01".to_owned()
}

fn default_key_variable() -> String {
    crate::credentials::DEFAULT_KEY_VARIABLE.to_owned()
}

const fn default_request_timeout_ms() -> u64 {
    // Longer than the Ollama provider's minute: a large request to a busy hosted model can
    // legitimately take minutes, and the caller's own deadline still applies on top.
    300_000
}

const fn default_max_output_tokens() -> u32 {
    4096
}

/// The setting that used to configure this provider's own retrying.
///
/// See [`AnthropicSettings::read`] for why it is answered by name rather than left to
/// `deny_unknown_fields`.
const MOVED_RETRY_KEY: &str = "max_retries";

/// Keys that would put a secret in the configuration tree.
///
/// Rejected by name, before serde sees the section, so the failure says what to do instead
/// rather than "unknown field".
const SECRET_KEYS: &[&str] = &["api_key", "apikey", "auth_token", "token", "secret", "key"];

/// Settings read from a component's configuration section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnthropicSettings {
    /// The base URL of the API.
    ///
    /// Must be `https`, unless it points at loopback — which exists so a test, a recording
    /// proxy or a local gateway can stand in for the service. Sending an API key in
    /// cleartext to anywhere else is refused rather than warned about.
    pub endpoint: String,
    /// The value of the `anthropic-version` header.
    pub api_version: String,
    /// The environment variable the API key is read from, when no file is configured.
    pub api_key_env: String,
    /// A file containing the API key, which wins over the variable when set.
    ///
    /// Must not be readable by other users.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_file: Option<PathBuf>,
    /// How long a request may run before it is abandoned, unless a shorter
    /// [`ExecutionContext`](aik_api::execution::ExecutionContext) deadline applies.
    pub request_timeout_ms: u64,
    /// The `max_tokens` sent when a request does not set one itself.
    ///
    /// The Messages API requires the field, and
    /// [`CompletionRequest`](aik_api::model::CompletionRequest) has no equivalent — it is a
    /// provider-specific limit, so it lives here and can be overridden per request through
    /// [`parameters`](aik_api::model::CompletionRequest::parameters).
    pub max_output_tokens: u32,
}

impl Default for AnthropicSettings {
    fn default() -> Self {
        Self {
            endpoint: default_endpoint(),
            api_version: default_api_version(),
            api_key_env: default_key_variable(),
            api_key_file: None,
            request_timeout_ms: default_request_timeout_ms(),
            max_output_tokens: default_max_output_tokens(),
        }
    }
}

impl AnthropicSettings {
    /// Reads a component's section, refusing one that carries a secret.
    ///
    /// The refusal is the point. `deny_unknown_fields` would already stop `api_key`, but with
    /// a message that reads like a typo, and a reader who took it that way might reach for
    /// the environment layer — `AIK_COMPONENTS__MODEL_ANTHROPIC__API_KEY` — which puts the
    /// same secret in the same tree by another route. So the keys are named and answered
    /// specifically, and nothing in this crate can be configured with a key inline.
    pub fn read(section: &Config) -> Result<Self> {
        for key in SECRET_KEYS {
            if section.contains(key) {
                return Err(Error::config(
                    (*key).to_string(),
                    format!(
                        "an API key must not be written into the configuration tree, which is \
                         cloned, merged and printed throughout the process; name where the key \
                         lives instead, with {}",
                        crate::credentials::key_source_hint()
                    ),
                ));
            }
        }
        if section.contains(MOVED_RETRY_KEY) {
            // `deny_unknown_fields` would already refuse this, with a message that reads like
            // a typo. A deployment that had configured retrying and was told "unknown field"
            // could reasonably conclude it had never worked, rather than that it moved.
            return Err(Error::config(
                MOVED_RETRY_KEY.to_string(),
                "retrying is no longer this provider's own concern; it is configured once for \
                 every provider under `components.model.resilient.retry`",
            ));
        }
        let settings: Self = section.get_or_default("")?;
        settings.validate()?;
        Ok(settings)
    }

    /// Checks what cannot be expressed in the types.
    fn validate(&self) -> Result<()> {
        validate_endpoint(&self.endpoint)?;
        header_safe(&self.api_version, "api_version")?;
        if self.api_key_env.trim().is_empty() {
            return Err(Error::config(
                "api_key_env",
                "the environment variable holding the API key cannot be an empty name",
            ));
        }
        if self.max_output_tokens == 0 {
            return Err(Error::config(
                "max_output_tokens",
                "a request that may produce no tokens has nothing to return",
            ));
        }
        Ok(())
    }

    /// The endpoint with any trailing slash removed.
    pub(crate) fn base_url(&self) -> String {
        self.endpoint.trim_end_matches('/').to_owned()
    }
}

/// Refuses an endpoint that would carry the key in cleartext to somewhere it can be read.
///
/// Loopback is allowed because it does not leave the machine, and because a stand-in server
/// is how this provider is tested at all. Everything else must be `https`.
fn validate_endpoint(endpoint: &str) -> Result<()> {
    let url = reqwest::Url::parse(endpoint)
        .map_err(|error| Error::config("endpoint", format!("not a URL: {error}")))?;

    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback(&url) => Ok(()),
        "http" => Err(Error::config(
            "endpoint",
            "refusing to send an API key over plain HTTP to a host that is not loopback; \
             use https",
        )),
        other => Err(Error::config(
            "endpoint",
            format!("`{other}` is not an HTTP scheme"),
        )),
    }
}

/// Whether a URL's host is this machine.
fn is_loopback(url: &reqwest::Url) -> bool {
    match url.host_str() {
        Some("localhost") => true,
        // An IPv6 host keeps its brackets in the URL's textual form.
        Some(host) => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback()),
        None => false,
    }
}

/// Rejects a configured header value that could not be sent, or could inject a header.
fn header_safe(value: &str, field: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
    {
        return Err(Error::config(
            field.to_owned(),
            "must be non-empty, visible ASCII text",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_configuration_that_still_sets_max_retries_is_told_where_it_went() {
        let section = Config::from_value(serde_json::json!({ "max_retries": 5 }));
        let error = AnthropicSettings::read(&section).unwrap_err();
        assert!(error.to_string().contains("max_retries"), "{error}");
        assert!(
            error.to_string().contains("model.resilient"),
            "the message must name where the setting moved to: {error}"
        );
    }
    use super::*;
    use aik_core::ErrorKind;
    use serde_json::json;

    fn section(value: serde_json::Value) -> Config {
        Config::from_value(value)
    }

    #[test]
    fn defaults_target_the_hosted_api() {
        let settings = AnthropicSettings::default();
        assert_eq!(settings.endpoint, "https://api.anthropic.com");
        assert_eq!(settings.api_version, "2023-06-01");
        assert_eq!(settings.api_key_env, "ANTHROPIC_API_KEY");
        assert_eq!(settings.api_key_file, None);
        settings.validate().unwrap();
    }

    #[test]
    fn an_empty_section_is_the_defaults() {
        let settings = AnthropicSettings::read(&section(json!({}))).unwrap();
        assert_eq!(settings, AnthropicSettings::default());
    }

    #[test]
    fn a_key_written_into_the_configuration_is_refused() {
        for key in SECRET_KEYS {
            let mut carrying = serde_json::Map::new();
            carrying.insert((*key).to_owned(), json!("sk-ant-secret"));
            let error =
                AnthropicSettings::read(&section(serde_json::Value::Object(carrying))).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Config);
            assert!(format!("{error}").contains("api_key_file"), "{error}");
            assert!(!format!("{error}").contains("sk-ant-secret"), "{error}");
        }
    }

    #[test]
    fn an_unknown_field_still_fails_at_startup() {
        // Not a secret, just a typo: it should stop the deployment rather than be ignored.
        assert!(AnthropicSettings::read(&section(json!({ "endpiont": "x" }))).is_err());
    }

    #[test]
    fn plain_http_is_refused_except_on_loopback() {
        assert!(validate_endpoint("http://api.anthropic.com").is_err());
        assert!(validate_endpoint("http://example.invalid:8080").is_err());
        assert!(validate_endpoint("http://127.0.0.1:1234").is_ok());
        assert!(validate_endpoint("http://[::1]:1234").is_ok());
        assert!(validate_endpoint("http://localhost:1234").is_ok());
        assert!(validate_endpoint("https://api.anthropic.com").is_ok());
    }

    #[test]
    fn a_non_http_endpoint_is_refused() {
        assert!(validate_endpoint("file:///etc/passwd").is_err());
        assert!(validate_endpoint("not a url").is_err());
    }

    #[test]
    fn a_header_value_cannot_carry_a_newline() {
        assert!(header_safe("2023-06-01", "api_version").is_ok());
        assert!(header_safe("2023-06-01\r\nx-evil: 1", "api_version").is_err());
        assert!(header_safe("", "api_version").is_err());
    }

    #[test]
    fn zero_output_tokens_is_refused() {
        let error =
            AnthropicSettings::read(&section(json!({ "max_output_tokens": 0 }))).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Config);
    }

    #[test]
    fn a_trailing_slash_is_normalised_away() {
        let settings = AnthropicSettings {
            endpoint: "https://api.anthropic.com/".to_owned(),
            ..AnthropicSettings::default()
        };
        assert_eq!(settings.base_url(), "https://api.anthropic.com");
    }
}

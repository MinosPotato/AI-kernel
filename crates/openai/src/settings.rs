//! Configuration for the OpenAI-compatible provider.
//!
//! Everything here is a *location* or a *limit*. The one thing that is neither — the API key
//! — is deliberately absent: see [`credentials`](crate::credentials) for why, and
//! [`OpenAiSettings::read`] for what happens to a configuration that tries anyway.

use std::path::PathBuf;

use aik_core::{Config, Error, Result};
use serde::{Deserialize, Serialize};

fn default_endpoint() -> String {
    "https://api.openai.com/v1".to_owned()
}

fn default_key_variable() -> String {
    crate::credentials::DEFAULT_KEY_VARIABLE.to_owned()
}

const fn default_request_timeout_ms() -> u64 {
    // The same five minutes the Anthropic provider allows, and for the same reason: a large
    // request to a busy hosted model can legitimately take minutes, and the caller's own
    // deadline still applies on top.
    300_000
}

/// Keys that would put a secret in the configuration tree.
///
/// Rejected by name, before serde sees the section, so the failure says what to do instead
/// rather than "unknown field".
const SECRET_KEYS: &[&str] = &["api_key", "apikey", "auth_token", "token", "secret", "key"];

/// The setting a reader might expect this provider to have, because two other providers do.
///
/// Answered by name rather than left to `deny_unknown_fields`, for the reason
/// [`OpenAiSettings::read`] gives.
const NOT_OURS_RETRY_KEY: &str = "max_retries";

/// Settings read from a component's configuration section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAiSettings {
    /// The base URL of the API, including whatever version prefix the server expects.
    ///
    /// The default is `https://api.openai.com/v1`. A server speaking the same dialect is
    /// named the same way — `https://openrouter.ai/api/v1`, `http://127.0.0.1:8000/v1` for a
    /// local vLLM — and the paths this provider appends (`chat/completions`, `models`,
    /// `embeddings`) are relative to it.
    ///
    /// Must be `https`, unless it points at loopback — which exists so a test, a recording
    /// proxy or a local inference server can stand in for the service. Sending an API key in
    /// cleartext to anywhere else is refused rather than warned about.
    pub endpoint: String,
    /// The environment variable the API key is read from, when no file is configured.
    pub api_key_env: String,
    /// A file containing the API key, which wins over the variable when set.
    ///
    /// Must not be readable by other users.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_file: Option<PathBuf>,
    /// Whether a missing key is a startup failure.
    ///
    /// `true` by default, because every hosted service that speaks this dialect requires
    /// one, and a deployment whose credential is missing should not come up, serve a session
    /// and fail on the first turn a person types.
    ///
    /// Setting it to `false` is what a local inference server needs — `llama.cpp`, `vLLM`
    /// and Ollama's own compatibility endpoint have no notion of an account — and is
    /// therefore only accepted for a loopback [`endpoint`](OpenAiSettings::endpoint). Off
    /// this machine, an unauthenticated request carrying the whole conversation is a
    /// configuration mistake far more often than it is a private gateway, and a deployment
    /// that really has one can hand it any key it likes.
    pub api_key_required: bool,
    /// The value of the `openai-organization` header, for an account that has more than one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// The value of the `openai-project` header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// How long a request may run before it is abandoned, unless a shorter
    /// [`ExecutionContext`](aik_api::execution::ExecutionContext) deadline applies.
    pub request_timeout_ms: u64,
}

impl Default for OpenAiSettings {
    fn default() -> Self {
        Self {
            endpoint: default_endpoint(),
            api_key_env: default_key_variable(),
            api_key_file: None,
            api_key_required: true,
            organization: None,
            project: None,
            request_timeout_ms: default_request_timeout_ms(),
        }
    }
}

impl OpenAiSettings {
    /// Reads a component's section, refusing one that carries a secret.
    ///
    /// The refusal is the point. `deny_unknown_fields` would already stop `api_key`, but with
    /// a message that reads like a typo, and a reader who took it that way might reach for
    /// the environment layer — `AIK_COMPONENTS__MODEL_OPENAI__API_KEY` — which puts the same
    /// secret in the same tree by another route. So the keys are named and answered
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
        if section.contains(NOT_OURS_RETRY_KEY) {
            // This provider never had its own retry loop, but the Anthropic one did, and a
            // deployment copying a section across would otherwise be told "unknown field"
            // and reasonably conclude retrying is not available at all.
            return Err(Error::config(
                NOT_OURS_RETRY_KEY.to_string(),
                "retrying is not a provider's own concern; it is configured once for every \
                 provider under `components.model.resilient.retry`",
            ));
        }
        let settings: Self = section.get_or_default("")?;
        settings.validate()?;
        Ok(settings)
    }

    /// Checks what cannot be expressed in the types.
    ///
    /// Crate-visible rather than private because [`OpenAiProvider::new`](crate::OpenAiProvider::new)
    /// calls it too. [`read`](OpenAiSettings::read) is not the only way to reach a provider —
    /// a caller can build this struct literally — and the endpoint check is the one that
    /// decides whether a credential goes out in cleartext, so it belongs on the path that
    /// attaches the credential rather than only on the path that parses configuration.
    pub(crate) fn validate(&self) -> Result<()> {
        let url = validate_endpoint(&self.endpoint)?;
        if self.api_key_env.trim().is_empty() {
            return Err(Error::config(
                "api_key_env",
                "the environment variable holding the API key cannot be an empty name",
            ));
        }
        if !self.api_key_required && !is_loopback(&url) {
            return Err(Error::config(
                "api_key_required",
                "refusing to send a conversation to a host that is not loopback without a \
                 credential; a gateway that authenticates some other way can still be given \
                 any key it accepts",
            ));
        }
        for (field, value) in [
            ("organization", self.organization.as_deref()),
            ("project", self.project.as_deref()),
        ] {
            if let Some(value) = value {
                header_safe(value, field)?;
            }
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
fn validate_endpoint(endpoint: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(endpoint)
        .map_err(|error| Error::config("endpoint", format!("not a URL: {error}")))?;

    match url.scheme() {
        "https" => Ok(url),
        "http" if is_loopback(&url) => Ok(url),
        "http" => Err(Error::config(
            "endpoint",
            "refusing to send an API key over plain HTTP to a host that is not loopback; use \
             https",
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
    use super::*;
    use aik_core::ErrorKind;
    use serde_json::json;

    fn section(value: serde_json::Value) -> Config {
        Config::from_value(value)
    }

    #[test]
    fn defaults_target_the_hosted_api() {
        let settings = OpenAiSettings::default();
        assert_eq!(settings.endpoint, "https://api.openai.com/v1");
        assert_eq!(settings.api_key_env, "OPENAI_API_KEY");
        assert_eq!(settings.api_key_file, None);
        assert!(settings.api_key_required);
        settings.validate().unwrap();
    }

    #[test]
    fn an_empty_section_is_the_defaults() {
        let settings = OpenAiSettings::read(&section(json!({}))).unwrap();
        assert_eq!(settings, OpenAiSettings::default());
    }

    #[test]
    fn a_key_written_into_the_configuration_is_refused() {
        for key in SECRET_KEYS {
            let mut carrying = serde_json::Map::new();
            carrying.insert((*key).to_owned(), json!("sk-secret"));
            let error =
                OpenAiSettings::read(&section(serde_json::Value::Object(carrying))).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Config);
            assert!(format!("{error}").contains("api_key_file"), "{error}");
            assert!(!format!("{error}").contains("sk-secret"), "{error}");
        }
    }

    #[test]
    fn a_configuration_that_sets_max_retries_is_told_where_retrying_lives() {
        let error = OpenAiSettings::read(&section(json!({ "max_retries": 5 }))).unwrap_err();
        assert!(error.to_string().contains("max_retries"), "{error}");
        assert!(error.to_string().contains("model.resilient"), "{error}");
    }

    #[test]
    fn an_unknown_field_still_fails_at_startup() {
        // Not a secret, just a typo: it should stop the deployment rather than be ignored.
        assert!(OpenAiSettings::read(&section(json!({ "endpiont": "x" }))).is_err());
    }

    #[test]
    fn plain_http_is_refused_except_on_loopback() {
        assert!(validate_endpoint("http://api.openai.com/v1").is_err());
        assert!(validate_endpoint("http://example.invalid:8080/v1").is_err());
        assert!(validate_endpoint("http://127.0.0.1:8000/v1").is_ok());
        assert!(validate_endpoint("http://[::1]:8000/v1").is_ok());
        assert!(validate_endpoint("http://localhost:11434/v1").is_ok());
        assert!(validate_endpoint("https://openrouter.ai/api/v1").is_ok());
    }

    #[test]
    fn a_non_http_endpoint_is_refused() {
        assert!(validate_endpoint("file:///etc/passwd").is_err());
        assert!(validate_endpoint("not a url").is_err());
    }

    #[test]
    fn an_unauthenticated_endpoint_is_allowed_only_on_loopback() {
        let local = OpenAiSettings::read(&section(json!({
            "endpoint": "http://127.0.0.1:8000/v1",
            "api_key_required": false,
        })))
        .unwrap();
        assert!(!local.api_key_required);

        let error = OpenAiSettings::read(&section(json!({
            "endpoint": "https://api.openai.com/v1",
            "api_key_required": false,
        })))
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Config);
        assert!(format!("{error}").contains("loopback"), "{error}");
    }

    #[test]
    fn a_header_value_cannot_carry_a_newline() {
        assert!(header_safe("org-abc", "organization").is_ok());
        assert!(header_safe("org\r\nx-evil: 1", "organization").is_err());
        assert!(header_safe("", "organization").is_err());
    }

    #[test]
    fn an_injected_organization_header_is_refused_at_startup() {
        let error = OpenAiSettings::read(&section(json!({
            "organization": "org\r\nx-evil: 1",
        })))
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Config);
    }

    #[test]
    fn a_trailing_slash_is_normalised_away() {
        let settings = OpenAiSettings {
            endpoint: "https://api.openai.com/v1/".to_owned(),
            ..OpenAiSettings::default()
        };
        assert_eq!(settings.base_url(), "https://api.openai.com/v1");
    }
}

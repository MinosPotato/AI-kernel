//! Configuration for the Ollama provider.

use serde::{Deserialize, Serialize};

fn default_endpoint() -> String {
    "http://localhost:11434".to_owned()
}

const fn default_request_timeout_ms() -> u64 {
    60_000
}

/// Settings read from a component's configuration section.
///
/// With no configuration at all, a provider talks to a default local Ollama install.
/// Overriding `endpoint` points it at a remote or containerised instance instead; nothing
/// else in the provider depends on where it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OllamaSettings {
    /// The base URL of the Ollama server, with no trailing slash required.
    pub endpoint: String,
    /// How long a request may run before it is abandoned, unless a shorter
    /// [`ExecutionContext`](aik_api::execution::ExecutionContext) deadline applies.
    pub request_timeout_ms: u64,
}

impl Default for OllamaSettings {
    fn default() -> Self {
        Self {
            endpoint: default_endpoint(),
            request_timeout_ms: default_request_timeout_ms(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_point_at_a_local_server() {
        let settings = OllamaSettings::default();
        assert_eq!(settings.endpoint, "http://localhost:11434");
        assert_eq!(settings.request_timeout_ms, 60_000);
    }

    #[test]
    fn absent_fields_fall_back_to_defaults() {
        let settings: OllamaSettings = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(settings, OllamaSettings::default());
    }
}

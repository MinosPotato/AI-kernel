//! What a deployment says a fetch may reach.

use std::time::Duration;

use serde::Deserialize;

/// The tool name used when none is given explicitly.
pub const DEFAULT_NAME: &str = "web.fetch";

/// The permission required when none is given explicitly.
pub const DEFAULT_PERMISSION: &str = "web.fetch";

/// The prefix of the resource naming the host a fetch reaches.
///
/// Two namespaces rather than one, exactly as `aik-exec` separates the program from the
/// command line: a rule about *which hosts may be reached at all* cannot be written by
/// accident as a rule about one URL, or the reverse.
pub const HOST_RESOURCE_PREFIX: &str = "host/";

/// The prefix of the resource naming the specific URL.
pub const URL_RESOURCE_PREFIX: &str = "url/";

/// How long one call may take when no [`ExecutionContext`](aik_api::execution::ExecutionContext)
/// deadline (or a later one) applies. Covers resolution, connection, every redirect hop and
/// the body together, not each of them.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long establishing one connection may take.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The most bytes one response body may occupy when no other limit is configured.
///
/// Sized for a model's context rather than for a browser: whatever arrives has to be paid
/// for as input tokens on every subsequent turn of the conversation it lands in.
pub const DEFAULT_MAX_BYTES: u64 = 512 * 1024;

/// How many redirects one call may follow when no other limit is configured.
pub const DEFAULT_MAX_REDIRECTS: usize = 3;

/// The `User-Agent` sent when a deployment names none.
pub const DEFAULT_USER_AGENT: &str = concat!("aik-net/", env!("CARGO_PKG_VERSION"));

/// The longest URL this tool will accept.
pub const MAX_URL_BYTES: usize = 2048;

/// What a deployment says about reaching the network, read from `agent.net`.
///
/// Every field defaults to the closed answer, so a deployment that writes nothing here and
/// registers the tool gets: `https` only, global addresses only, any public host, half a
/// megabyte, three redirects.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetSettings {
    /// Whether addresses on this machine or this network may be reached at all.
    ///
    /// Off by default. On, it admits loopback, the RFC 1918 ranges, carrier-grade NAT and
    /// IPv6 unique-local addresses — and nothing else: the link-local range where hosted
    /// machines answer with their own credentials stays refused. See
    /// [`crate::address`].
    pub allow_local_addresses: bool,
    /// Whether `http` URLs are accepted as well as `https`.
    ///
    /// Off by default, for the same reason `aik-anthropic` refuses a plaintext endpoint: a
    /// request that leaves this machine unencrypted is a configuration mistake far more
    /// often than it is a deliberate choice. A deployment fetching from a local service
    /// that has no certificate turns it on.
    pub allow_http: bool,
    /// The only hosts that may be reached. Empty means any host that resolves acceptably.
    ///
    /// An entry is matched against the URL's host, lowercased: `example.com` matches that
    /// host exactly, and `.example.com` matches it and every subdomain of it.
    pub allow_hosts: Vec<String>,
    /// Hosts that may never be reached, matched the same way, and applied before
    /// [`NetSettings::allow_hosts`].
    pub deny_hosts: Vec<String>,
    /// The largest response body one call will read, in bytes.
    pub max_bytes: Option<u64>,
    /// The wall-clock budget for one call, in milliseconds.
    pub timeout_ms: Option<u64>,
    /// How many redirects one call may follow.
    pub max_redirects: Option<usize>,
    /// The `User-Agent` header sent with every request.
    pub user_agent: Option<String>,
}

impl NetSettings {
    /// The configured body limit, or [`DEFAULT_MAX_BYTES`].
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes.unwrap_or(DEFAULT_MAX_BYTES)
    }

    /// The configured per-call budget, or [`DEFAULT_TIMEOUT`].
    pub fn timeout(&self) -> Duration {
        self.timeout_ms
            .map_or(DEFAULT_TIMEOUT, Duration::from_millis)
    }

    /// The configured redirect limit, or [`DEFAULT_MAX_REDIRECTS`].
    pub fn max_redirects(&self) -> usize {
        self.max_redirects.unwrap_or(DEFAULT_MAX_REDIRECTS)
    }

    /// The configured `User-Agent`, or [`DEFAULT_USER_AGENT`].
    pub fn user_agent(&self) -> &str {
        self.user_agent.as_deref().unwrap_or(DEFAULT_USER_AGENT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_defaults_to_the_closed_answer() {
        let settings = NetSettings::default();
        assert!(!settings.allow_local_addresses);
        assert!(!settings.allow_http);
        assert_eq!(settings.max_bytes(), DEFAULT_MAX_BYTES);
        assert_eq!(settings.max_redirects(), DEFAULT_MAX_REDIRECTS);
        assert_eq!(settings.timeout(), DEFAULT_TIMEOUT);
    }

    #[test]
    fn an_unknown_field_is_a_configuration_error_rather_than_a_silent_default() {
        let error = serde_json::from_value::<NetSettings>(serde_json::json!({
            "allow_localhost": true
        }))
        .unwrap_err();
        assert!(error.to_string().contains("allow_localhost"));
    }

    #[test]
    fn a_deployment_can_widen_every_bound_it_is_allowed_to() {
        let settings: NetSettings = serde_json::from_value(serde_json::json!({
            "allow_local_addresses": true,
            "allow_http": true,
            "allow_hosts": [".example.com"],
            "max_bytes": 1024,
            "timeout_ms": 500,
            "max_redirects": 0,
            "user_agent": "example/1"
        }))
        .unwrap();
        assert!(settings.allow_local_addresses);
        assert!(settings.allow_http);
        assert_eq!(settings.max_bytes(), 1024);
        assert_eq!(settings.timeout(), Duration::from_millis(500));
        assert_eq!(settings.max_redirects(), 0);
        assert_eq!(settings.user_agent(), "example/1");
    }
}

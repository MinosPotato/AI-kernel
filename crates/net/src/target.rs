//! Turning the model's `url` argument into something with a decided shape.
//!
//! Everything here is syntax and configuration: what a URL is allowed to look like, and
//! which hosts this deployment named. Nothing here touches the network, and nothing here is
//! an authorization decision — a URL that passes every check in this module has only earned
//! the right to be *asked about*, by [`crate::WebFetchTool`], through the registry.

use aik_core::{Error, Result};
use url::{Host, Url};

use crate::settings::{MAX_URL_BYTES, NetSettings};

/// A URL that has passed every check that does not require the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    /// The URL itself, normalised by the parser.
    pub(crate) url: Url,
    /// The host, lowercased, with an IPv6 literal written without its brackets.
    pub(crate) host: String,
}

impl Target {
    /// The form a policy is asked about and the audit trail keeps.
    ///
    /// Deliberately **not** the whole URL: the query string is dropped. A resource travels
    /// into the durable audit trail, and a query string is where session tokens, signed
    /// links and unbounded model-authored text live — none of which a rule can usefully
    /// match on and all of which would then be kept forever. The scheme, host, port and
    /// path are what a deployment actually writes rules about.
    pub(crate) fn resource(&self) -> String {
        let mut trimmed = self.url.clone();
        trimmed.set_query(None);
        trimmed.set_fragment(None);
        trimmed.to_string()
    }
}

/// Checks one URL against everything knowable without resolving it.
pub(crate) fn validate(raw: &str, settings: &NetSettings) -> Result<Target> {
    if raw.len() > MAX_URL_BYTES {
        return Err(Error::InvalidArgument(format!(
            "url is {} bytes, over the {MAX_URL_BYTES}-byte limit",
            raw.len()
        )));
    }

    let url = Url::parse(raw)
        .map_err(|error| Error::InvalidArgument(format!("`{raw}` is not a valid URL: {error}")))?;
    validate_url(&url, settings)
}

/// The same checks, for a URL that was produced rather than typed — a redirect's target.
pub(crate) fn validate_url(url: &Url, settings: &NetSettings) -> Result<Target> {
    match url.scheme() {
        "https" => {}
        "http" if settings.allow_http => {}
        "http" => {
            return Err(Error::Confinement(
                "`http` URLs are refused unless the deployment sets `allow_http`; \
                 the request would leave this machine unencrypted"
                    .to_owned(),
            ));
        }
        other => {
            return Err(Error::Confinement(format!(
                "scheme `{other}` is not fetchable; only http and https are"
            )));
        }
    }

    // A URL carrying credentials is either an attempt to authenticate as somebody, or an
    // attempt to make the host look like something it is not (`https://example.com@evil`).
    // Neither is a fetch this tool performs.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::Confinement(
            "a URL carrying a username or password is refused".to_owned(),
        ));
    }

    let host = match url.host() {
        Some(Host::Domain(domain)) => domain.to_ascii_lowercase(),
        Some(Host::Ipv4(address)) => address.to_string(),
        Some(Host::Ipv6(address)) => address.to_string(),
        None => {
            return Err(Error::InvalidArgument("the URL names no host".to_owned()));
        }
    };
    if host.is_empty() {
        return Err(Error::InvalidArgument("the URL names no host".to_owned()));
    }

    // A privileged port other than the two this tool speaks is not a document: it is
    // whatever protocol lives there, being handed an HTTP request in the hope that some of
    // it parses. Above 1024 the address checks are what stand between a call and a service,
    // and they are the checks that matter.
    if let Some(port) = url.port()
        && port < 1024
        && port != 80
        && port != 443
    {
        return Err(Error::Confinement(format!(
            "port {port} is refused; a privileged port other than 80 or 443 is not a fetch target"
        )));
    }

    if matches(&settings.deny_hosts, &host) {
        return Err(Error::Confinement(format!(
            "`{host}` is in this deployment's denied hosts"
        )));
    }
    if !settings.allow_hosts.is_empty() && !matches(&settings.allow_hosts, &host) {
        return Err(Error::Confinement(format!(
            "`{host}` is not in this deployment's allowed hosts"
        )));
    }

    Ok(Target {
        url: url.clone(),
        host,
    })
}

/// Whether `host` matches any entry, where a leading `.` also matches subdomains.
fn matches(entries: &[String], host: &str) -> bool {
    entries.iter().any(|entry| {
        let entry = entry.trim().to_ascii_lowercase();
        match entry.strip_prefix('.') {
            Some(domain) => host == domain || host.ends_with(&format!(".{domain}")),
            None => host == entry,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::ErrorKind;

    fn open() -> NetSettings {
        NetSettings::default()
    }

    #[test]
    fn an_ordinary_https_url_passes() {
        let target = validate("https://example.com/a/b?q=1#frag", &open()).unwrap();
        assert_eq!(target.host, "example.com");
        assert_eq!(target.resource(), "https://example.com/a/b");
    }

    #[test]
    fn the_resource_drops_the_query_so_a_token_in_one_is_not_kept_forever() {
        let target = validate("https://example.com/cb?token=secret", &open()).unwrap();
        assert!(!target.resource().contains("secret"));
    }

    #[test]
    fn plaintext_is_refused_until_a_deployment_asks_for_it() {
        let error = validate("http://example.com/", &open()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Confinement);

        let settings = NetSettings {
            allow_http: true,
            ..NetSettings::default()
        };
        assert!(validate("http://example.com/", &settings).is_ok());
    }

    #[test]
    fn only_http_and_https_are_fetchable() {
        for raw in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "gopher://example.com/",
            "data:text/plain,hello",
        ] {
            let error = validate(raw, &open()).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Confinement, "{raw}");
        }
    }

    #[test]
    fn a_url_carrying_credentials_is_refused() {
        // Also the shape that makes `evil.example` look like `example.com` at a glance.
        let error = validate("https://example.com@evil.example/", &open()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Confinement);
    }

    #[test]
    fn privileged_ports_other_than_the_web_ones_are_refused() {
        for raw in [
            "https://example.com:22/",
            "https://example.com:25/",
            "https://example.com:6379/x",
        ] {
            let outcome = validate(raw, &open());
            if raw.contains(":6379") {
                assert!(outcome.is_ok(), "{raw} is unprivileged and allowed");
            } else {
                assert_eq!(outcome.unwrap_err().kind(), ErrorKind::Confinement, "{raw}");
            }
        }
        assert!(validate("https://example.com:443/", &open()).is_ok());
        assert!(
            validate(
                "http://example.com:80/",
                &NetSettings {
                    allow_http: true,
                    ..NetSettings::default()
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn an_allow_list_excludes_everything_it_does_not_name() {
        let settings = NetSettings {
            allow_hosts: vec!["docs.example.com".to_owned(), ".wiki.test".to_owned()],
            ..NetSettings::default()
        };
        assert!(validate("https://docs.example.com/x", &settings).is_ok());
        assert!(validate("https://wiki.test/x", &settings).is_ok());
        assert!(validate("https://a.b.wiki.test/x", &settings).is_ok());
        assert!(validate("https://example.com/x", &settings).is_err());
        assert!(validate("https://evil-docs.example.com/x", &settings).is_err());
        // The suffix must be a label boundary, not a string one.
        assert!(validate("https://notwiki.test/x", &settings).is_err());
    }

    #[test]
    fn a_deny_list_wins_over_an_allow_list() {
        let settings = NetSettings {
            allow_hosts: vec![".example.com".to_owned()],
            deny_hosts: vec!["secret.example.com".to_owned()],
            ..NetSettings::default()
        };
        assert!(validate("https://docs.example.com/", &settings).is_ok());
        assert!(validate("https://secret.example.com/", &settings).is_err());
    }

    #[test]
    fn host_matching_is_case_insensitive_on_both_sides() {
        let settings = NetSettings {
            allow_hosts: vec!["Docs.Example.COM".to_owned()],
            ..NetSettings::default()
        };
        assert!(validate("https://DOCS.example.com/", &settings).is_ok());
    }

    #[test]
    fn an_over_long_url_is_refused_before_it_is_parsed() {
        let raw = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
        assert_eq!(
            validate(&raw, &open()).unwrap_err().kind(),
            ErrorKind::InvalidArgument
        );
    }

    #[test]
    fn a_relative_reference_is_not_a_url() {
        assert_eq!(
            validate("/etc/passwd", &open()).unwrap_err().kind(),
            ErrorKind::InvalidArgument
        );
    }
}

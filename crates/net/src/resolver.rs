//! Name resolution, and the guarantee that the address connected to is the address checked.
//!
//! A name is not a destination. Between deciding that `wiki.example.com` is acceptable and
//! the `connect(2)` that follows, the answer can change — that is not a hypothetical, it is
//! how DNS rebinding works: a name whose record has a one-second lifetime resolves to a
//! public address for the check and to `127.0.0.1` for the connection, and every check
//! written against the *name* passes.
//!
//! So the check is written against the address, and it happens twice, in two places that do
//! not depend on each other:
//!
//! 1. [`allowed_addresses`] resolves the host before anything is sent, so a refusal can say
//!    which range the address was in. This is the message the model and the audit trail get.
//! 2. [`GuardedResolver`] is the only resolver the HTTP client has. Every address the client
//!    ever connects to comes out of it, and it returns none that
//!    [`classify`](crate::address::classify) refuses. A record that changes between (1) and
//!    (2) — or between one redirect hop and the next — is therefore caught at the point of
//!    use rather than at the point of checking.
//!
//! (1) alone would be a check with a race in it. (2) alone would be a guarantee with an
//! unreadable failure message. Together the message comes from the first and the property
//! comes from the second.

use std::net::{IpAddr, SocketAddr};

use aik_core::{Error, Result};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use crate::address::{AddressClass, classify};

/// Resolves `host` and keeps only the addresses a fetch may reach.
///
/// Returns [`Error::Confinement`] when the name resolved but every address it named is
/// refused, and anything else when it did not resolve at all. That distinction is the one
/// worth keeping: a deployment reading an audit trail must be able to tell "this name points
/// somewhere it may not go" from "this name could not be looked up". Which *kind* of lookup
/// failure it was is not reported, because the platform does not reliably say: a host that
/// does not exist and a resolver that is down both arrive as an uncategorised `getaddrinfo`
/// failure, and inventing a distinction between them would be a guess in an audit trail.
pub(crate) async fn allowed_addresses(host: &str, allow_local: bool) -> Result<Vec<IpAddr>> {
    let resolved = resolve_host(host).await?;
    let (allowed, refused) = partition(resolved, allow_local);

    if allowed.is_empty() {
        let detail = refused.first().map_or_else(
            || "no addresses".to_owned(),
            |(address, class)| format!("{address} is in {}", class.range()),
        );
        return Err(Error::Confinement(format!(
            "`{host}` resolves only to addresses this deployment may not reach ({detail})"
        )));
    }
    Ok(allowed)
}

async fn resolve_host(host: &str) -> Result<Vec<IpAddr>> {
    // Port zero: this is a name lookup, and the port the connection eventually uses comes
    // from the URL.
    let resolved =
        tokio::net::lookup_host((host, 0))
            .await
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => Error::not_found("host", host),
                _ => Error::wrap(format!("resolving `{host}`"), error),
            })?;
    let addresses: Vec<IpAddr> = resolved.map(|socket| socket.ip()).collect();
    if addresses.is_empty() {
        return Err(Error::not_found("host", host));
    }
    Ok(addresses)
}

fn partition(
    addresses: Vec<IpAddr>,
    allow_local: bool,
) -> (Vec<IpAddr>, Vec<(IpAddr, AddressClass)>) {
    let mut allowed = Vec::new();
    let mut refused = Vec::new();
    for address in addresses {
        let class = classify(address);
        if class.is_reachable(allow_local) {
            allowed.push(address);
        } else {
            refused.push((address, class));
        }
    }
    (allowed, refused)
}

/// The HTTP client's only resolver: one that cannot return an address a fetch may not reach.
///
/// Filtering rather than refusing the whole answer is deliberate. A name that resolves to
/// one public and one private address is a real and ordinary thing — a split-horizon zone,
/// a host with an interface on both — and connecting to the public one is exactly right.
/// What must not happen is connecting to the other, and that is a property of what this
/// returns, not of how many entries the record had.
#[derive(Debug, Clone)]
pub(crate) struct GuardedResolver {
    allow_local: bool,
}

impl GuardedResolver {
    pub(crate) fn new(allow_local: bool) -> Self {
        Self { allow_local }
    }
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_local = self.allow_local;
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addresses = resolve_host(&host)
                .await
                .map_err(Box::<dyn std::error::Error + Send + Sync>::from)?;
            let (allowed, refused) = partition(addresses, allow_local);
            if allowed.is_empty() {
                let detail = refused.first().map_or_else(
                    || "no addresses".to_owned(),
                    |(address, class)| format!("{address} is in {}", class.range()),
                );
                return Err(Box::<dyn std::error::Error + Send + Sync>::from(format!(
                    "refusing to connect to `{host}`: {detail}"
                )));
            }
            let addrs: Addrs = Box::new(
                allowed
                    .into_iter()
                    .map(|address| SocketAddr::new(address, 0))
                    .collect::<Vec<_>>()
                    .into_iter(),
            );
            Ok(addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::ErrorKind;

    #[test]
    fn a_mixed_answer_keeps_only_what_may_be_reached() {
        let addresses = vec![
            "127.0.0.1".parse().unwrap(),
            "1.1.1.1".parse().unwrap(),
            "169.254.169.254".parse().unwrap(),
        ];
        let (allowed, refused) = partition(addresses, false);
        assert_eq!(allowed, vec!["1.1.1.1".parse::<IpAddr>().unwrap()]);
        assert_eq!(refused.len(), 2);
    }

    #[test]
    fn opting_into_local_addresses_does_not_opt_into_the_metadata_range() {
        let addresses = vec![
            "127.0.0.1".parse().unwrap(),
            "169.254.169.254".parse().unwrap(),
        ];
        let (allowed, refused) = partition(addresses, true);
        assert_eq!(allowed, vec!["127.0.0.1".parse::<IpAddr>().unwrap()]);
        assert_eq!(refused.len(), 1);
    }

    #[tokio::test]
    async fn an_ip_literal_that_is_refused_names_the_range_it_is_in() {
        let error = allowed_addresses("127.0.0.1", false).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Confinement);
        assert!(error.to_string().contains("127.0.0.0/8"), "{error}");
    }

    #[tokio::test]
    async fn an_ip_literal_that_is_allowed_resolves_to_itself() {
        let addresses = allowed_addresses("127.0.0.1", true).await.unwrap();
        assert_eq!(addresses, vec!["127.0.0.1".parse::<IpAddr>().unwrap()]);
    }

    #[tokio::test]
    async fn a_name_that_does_not_resolve_is_not_reported_as_a_refusal() {
        // `.invalid` is reserved by RFC 2606 precisely so that it never resolves. The point
        // of the assertion is the distinction: a name that could not be looked up must not
        // read, afterwards, as a name that pointed somewhere it was not allowed to go.
        let error = allowed_addresses("nothing.invalid", true)
            .await
            .unwrap_err();
        assert_ne!(error.kind(), ErrorKind::Confinement);
        assert!(error.to_string().contains("nothing.invalid"), "{error}");
    }
}

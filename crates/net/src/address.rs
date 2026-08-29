//! Deciding what an IP address is, independently of who asked for it.
//!
//! This is the whole of the confinement boundary's knowledge about addresses. Nothing here
//! consults policy, a principal, or a host name: it answers one question — *is this address
//! somewhere a fetch may legitimately go* — and it answers it the same way whoever is asking.
//!
//! The classification is deliberately coarse, three-valued, and closed:
//!
//! * [`AddressClass::Global`] is the only class reachable by default.
//! * [`AddressClass::Local`] is everything that belongs to this machine or this network —
//!   loopback, the RFC 1918 ranges, carrier-grade NAT, IPv6 unique-local addresses. A
//!   deployment that means to reach a service on its own network can turn these on
//!   ([`NetSettings::allow_local_addresses`](crate::NetSettings::allow_local_addresses)),
//!   because "fetch from the wiki on the LAN" is a real deployment and not a mistake.
//! * [`AddressClass::Refused`] is everything no configuration can enable. Two kinds of thing
//!   land here: addresses that are not a fetch target under any reading (the unspecified
//!   address, multicast, broadcast, the documentation and benchmarking ranges), and
//!   addresses whose only role in a fetch is to defeat a check — the link-local range that
//!   holds every cloud provider's instance-credential endpoint, and the IPv6 forms that
//!   embed an IPv4 address inside something that does not look like one.
//!
//! Anything unrecognised is [`AddressClass::Global`], which is the direction that matters:
//! the classes above are exhaustively enumerated ranges, so a new one appearing means a
//! *public* address is treated as public. The alternative — an allowlist of global ranges —
//! would refuse the internet every time IANA allocated a block.
//!
//! # Why link-local is refused rather than merely local
//!
//! `169.254.169.254` answers, on most hosted machines, with credentials for the account the
//! machine runs as. It is the single most exploited destination in server-side request
//! forgery, it is never a document anybody meant to fetch, and a deployment that turned
//! local addresses on in order to reach a wiki at `10.0.0.5` did not thereby mean to hand a
//! model the instance's role credentials. Making it a separate, unconditional refusal keeps
//! the two decisions apart. `fe80::/10` is refused for the same reason and for symmetry:
//! the address a check is written against should not depend on which family it arrived in.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// What kind of address something is, and therefore whether a fetch may reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressClass {
    /// A globally routable address. Reachable.
    Global,
    /// An address belonging to this machine or this network. Reachable only when a
    /// deployment opts in; the string names the range, for the refusal message.
    Local(&'static str),
    /// An address no configuration makes reachable; the string names the range.
    Refused(&'static str),
}

impl AddressClass {
    /// Whether an address of this class may be connected to under `allow_local`.
    pub fn is_reachable(self, allow_local: bool) -> bool {
        match self {
            Self::Global => true,
            Self::Local(_) => allow_local,
            Self::Refused(_) => false,
        }
    }

    /// The name of the range, for a message explaining a refusal.
    pub fn range(self) -> &'static str {
        match self {
            Self::Global => "a global address",
            Self::Local(range) | Self::Refused(range) => range,
        }
    }
}

/// Classifies one address.
pub fn classify(address: IpAddr) -> AddressClass {
    match address {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => classify_v6(v6),
    }
}

fn classify_v4(address: Ipv4Addr) -> AddressClass {
    let [a, b, c, _] = address.octets();

    if address.is_unspecified() || a == 0 {
        return AddressClass::Refused("the \"this network\" range 0.0.0.0/8");
    }
    if address.is_broadcast() {
        return AddressClass::Refused("the broadcast address");
    }
    if address.is_multicast() {
        return AddressClass::Refused("the multicast range 224.0.0.0/4");
    }
    if address.is_link_local() {
        // Where instance credentials live. See the module documentation.
        return AddressClass::Refused("the link-local range 169.254.0.0/16");
    }
    if address.is_documentation() {
        return AddressClass::Refused("a documentation range");
    }
    // 192.0.0.0/24, IETF protocol assignments.
    if a == 192 && b == 0 && c == 0 {
        return AddressClass::Refused("the IETF protocol assignment range 192.0.0.0/24");
    }
    // 198.18.0.0/15, benchmarking.
    if a == 198 && (b == 18 || b == 19) {
        return AddressClass::Refused("the benchmarking range 198.18.0.0/15");
    }
    // 240.0.0.0/4, reserved for future use. The broadcast address is already handled.
    if a >= 240 {
        return AddressClass::Refused("the reserved range 240.0.0.0/4");
    }
    if address.is_loopback() {
        return AddressClass::Local("the loopback range 127.0.0.0/8");
    }
    if address.is_private() {
        return AddressClass::Local("a private range (RFC 1918)");
    }
    // 100.64.0.0/10, carrier-grade NAT.
    if a == 100 && (64..128).contains(&b) {
        return AddressClass::Local("the carrier-grade NAT range 100.64.0.0/10");
    }
    AddressClass::Global
}

fn classify_v6(address: Ipv6Addr) -> AddressClass {
    // An IPv4 address wearing an IPv6 spelling is still that IPv4 address, and the check it
    // is subject to must not depend on which of the two it arrived as.
    if let Some(v4) = address.to_ipv4_mapped() {
        return classify_v4(v4);
    }
    // `to_ipv4` also matches the deprecated `::a.b.c.d` compatible form, which is the same
    // trick without the mapping prefix. Nothing legitimate produces one today, so it is
    // refused outright rather than unwrapped.
    if address.to_ipv4().is_some() && !address.is_unspecified() && !address.is_loopback() {
        return AddressClass::Refused("a deprecated IPv4-compatible address");
    }

    let segments = address.segments();

    if address.is_unspecified() {
        return AddressClass::Refused("the unspecified address ::");
    }
    if address.is_multicast() {
        return AddressClass::Refused("the multicast range ff00::/8");
    }
    // fe80::/10, link-local. Refused for the same reason as 169.254.0.0/16.
    if segments[0] & 0xffc0 == 0xfe80 {
        return AddressClass::Refused("the link-local range fe80::/10");
    }
    // 2002::/16, 6to4 — an embedded IPv4 address again.
    if segments[0] == 0x2002 {
        return AddressClass::Refused("the 6to4 range 2002::/16");
    }
    // 2001::/32, Teredo — likewise.
    if segments[0] == 0x2001 && segments[1] == 0 {
        return AddressClass::Refused("the Teredo range 2001::/32");
    }
    // 2001:db8::/32, documentation.
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return AddressClass::Refused("the documentation range 2001:db8::/32");
    }
    // 100::/64, the discard-only range.
    if segments[0] == 0x0100 && segments[1..4] == [0, 0, 0] {
        return AddressClass::Refused("the discard-only range 100::/64");
    }
    if address.is_loopback() {
        return AddressClass::Local("the loopback address ::1");
    }
    // fc00::/7, unique local.
    if segments[0] & 0xfe00 == 0xfc00 {
        return AddressClass::Local("the unique-local range fc00::/7");
    }
    AddressClass::Global
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(raw: &str) -> AddressClass {
        classify(raw.parse().expect("a parseable address"))
    }

    #[test]
    fn ordinary_public_addresses_are_global() {
        for raw in ["1.1.1.1", "93.184.216.34", "2606:4700::1111", "8.8.8.8"] {
            assert_eq!(class(raw), AddressClass::Global, "{raw}");
        }
    }

    #[test]
    fn the_machines_own_networks_are_local_rather_than_refused() {
        for raw in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "100.64.0.1",
            "::1",
            "fd00::1",
        ] {
            assert!(
                matches!(class(raw), AddressClass::Local(_)),
                "{raw} should be local, got {:?}",
                class(raw)
            );
        }
    }

    #[test]
    fn the_metadata_range_is_refused_and_not_merely_local() {
        // The one case where "the deployment opted into its own network" must not be
        // enough: this is where a hosted machine's credentials answer.
        assert!(matches!(class("169.254.169.254"), AddressClass::Refused(_)));
        assert!(!class("169.254.169.254").is_reachable(true));
        assert!(matches!(class("fe80::1"), AddressClass::Refused(_)));
    }

    #[test]
    fn addresses_that_are_never_a_fetch_target_are_refused() {
        for raw in [
            "0.0.0.0",
            "0.1.2.3",
            "255.255.255.255",
            "224.0.0.1",
            "192.0.2.1",
            "198.51.100.7",
            "203.0.113.9",
            "192.0.0.1",
            "198.18.0.1",
            "240.0.0.1",
            "::",
            "ff02::1",
            "2001:db8::1",
            "2002::1",
            "2001:0:1::1",
            "100::1",
        ] {
            assert!(
                matches!(class(raw), AddressClass::Refused(_)),
                "{raw} should be refused, got {:?}",
                class(raw)
            );
        }
    }

    #[test]
    fn an_ipv4_address_in_ipv6_clothing_is_classified_as_the_ipv4_address() {
        // Otherwise `::ffff:127.0.0.1` is a spelling that walks past a v4-only check.
        assert!(matches!(class("::ffff:127.0.0.1"), AddressClass::Local(_)));
        assert!(matches!(
            class("::ffff:169.254.169.254"),
            AddressClass::Refused(_)
        ));
        assert_eq!(class("::ffff:1.1.1.1"), AddressClass::Global);
    }

    #[test]
    fn the_deprecated_compatible_form_is_refused_outright() {
        assert!(matches!(
            class("::1.2.3.4"),
            AddressClass::Refused("a deprecated IPv4-compatible address")
        ));
    }

    #[test]
    fn local_addresses_are_reachable_only_when_a_deployment_says_so() {
        assert!(!class("127.0.0.1").is_reachable(false));
        assert!(class("127.0.0.1").is_reachable(true));
        assert!(class("1.1.1.1").is_reachable(false));
    }
}

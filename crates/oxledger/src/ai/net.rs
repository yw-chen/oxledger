// SPDX-License-Identifier: GPL-2.0-or-later
//! Address checks for every connection to an AI endpoint (design 05
//! §3.13.2 P-3 (2)(3), appendix D V1).
//!
//! The host is resolved, every address is checked, and the connection is
//! pinned to a checked address (TLS SNI and `Host` keep the registered
//! name), so a second resolution (DNS rebinding) cannot move it.

use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::pin::Pin;

/// Resolves a host name. The server uses the system resolver; tests inject
/// a fake one.
pub trait Resolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        port: u16,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>>;
}

/// `tokio::net::lookup_host`.
pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        port: u16,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
        Box::pin(async move {
            let addrs = tokio::net::lookup_host((host, port)).await?;
            Ok(addrs.map(|a| a.ip()).collect())
        })
    }
}

/// Cloud metadata addresses (not all of them are link-local).
pub const METADATA: [IpAddr; 5] = [
    IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
    IpAddr::V4(Ipv4Addr::new(169, 254, 170, 2)),
    IpAddr::V4(Ipv4Addr::new(100, 100, 100, 200)),
    IpAddr::V4(Ipv4Addr::new(192, 0, 0, 192)),
    IpAddr::V6(Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254)),
];

/// What an address is, for P-3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AddrClass {
    Public,
    Private,
    Loopback,
    /// Unspecified, multicast, broadcast, link-local or metadata: never.
    Blocked,
}

/// IPv4-mapped IPv6 is judged as the IPv4 address.
pub fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

/// The IPv4 address an IPv6 translation form carries: IPv4-translated
/// `::ffff:0:0:0/96`, IPv4-compatible `::/96` (not `::` or `::1`), NAT64
/// `64:ff9b::/96` and 6to4 `2002::/16` (IPv4-mapped is [`normalize`]d).
fn embedded_v4(a: Ipv6Addr) -> Option<Ipv4Addr> {
    let last =
        |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    match a.segments() {
        [0, 0, 0, 0, 0xffff, 0, hi, lo] => Some(last(hi, lo)),
        [0, 0, 0, 0, 0, 0, hi, lo] if (hi, lo) != (0, 0) && (hi, lo) != (0, 1) => {
            Some(last(hi, lo))
        }
        [0x64, 0xff9b, 0, 0, 0, 0, hi, lo] => Some(last(hi, lo)),
        [0x2002, hi, lo, ..] => Some(last(hi, lo)),
        _ => None,
    }
}

pub fn classify(ip: IpAddr) -> AddrClass {
    let ip = normalize(ip);
    // A translation form is judged as the IPv4 address it carries, and only
    // a public one passes: a local provider is registered by its own
    // address, never through a translator (independent verification D5).
    if let IpAddr::V6(v6) = ip
        && let Some(v4) = embedded_v4(v6)
    {
        return match classify(IpAddr::V4(v4)) {
            AddrClass::Public => AddrClass::Public,
            _ => AddrClass::Blocked,
        };
    }
    if METADATA.contains(&ip) {
        return AddrClass::Blocked;
    }
    match ip {
        IpAddr::V4(a) => {
            let o = a.octets();
            if a.is_unspecified()
                || o[0] == 0
                || a.is_multicast()
                || a.is_broadcast()
                || a.is_link_local()
                || o[0] >= 240
            {
                AddrClass::Blocked
            } else if a.is_loopback() {
                AddrClass::Loopback
            } else if a.is_private() || (o[0] == 100 && (o[1] & 0xc0) == 64) {
                AddrClass::Private
            } else {
                AddrClass::Public
            }
        }
        IpAddr::V6(a) => {
            let s = a.segments();
            // Link-local fe80::/10 and the deprecated site-local fec0::/10;
            // NAT64 local-use 64:ff9b:1::/48 and Teredo 2001::/32 carry an
            // IPv4 address that cannot be read reliably.
            if a.is_unspecified()
                || a.is_multicast()
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] & 0xffc0) == 0xfec0
                || (s[0], s[1], s[2]) == (0x64, 0xff9b, 1)
                || (s[0], s[1]) == (0x2001, 0)
            {
                AddrClass::Blocked
            } else if a.is_loopback() {
                AddrClass::Loopback
            } else if (s[0] & 0xfe00) == 0xfc00 {
                AddrClass::Private
            } else {
                AddrClass::Public
            }
        }
    }
}

/// P-3 (3): may a connection over `https` (else http) go to `ip`?
pub fn permitted(ip: IpAddr, https: bool, allow_http: bool) -> bool {
    match classify(ip) {
        AddrClass::Blocked => false,
        AddrClass::Public => https,
        AddrClass::Private | AddrClass::Loopback => https || allow_http,
    }
}

/// The connection check failed (the address is not named anywhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The name did not resolve.
    Unresolved,
    /// An address is not allowed (P-3).
    Blocked,
}

/// Resolves `host` and checks every address; the connection goes to the
/// first one. Any address that is not allowed refuses the whole name (one
/// bad answer is enough to suspect the name).
pub async fn pick(
    resolver: &dyn Resolver,
    host: &str,
    port: u16,
    https: bool,
    allow_http: bool,
) -> Result<IpAddr, Refused> {
    let addrs = match host.parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => resolver
            .resolve(host, port)
            .await
            .map_err(|_| Refused::Unresolved)?,
    };
    if addrs.is_empty() {
        return Err(Refused::Unresolved);
    }
    if addrs.iter().any(|a| !permitted(*a, https, allow_http)) {
        return Err(Refused::Blocked);
    }
    Ok(normalize(addrs[0]))
}

/// Where a registered endpoint resolves now (for the settings page): the
/// least permissive class among its addresses, or `None` when it does not
/// resolve.
pub async fn class_of(resolver: &dyn Resolver, host: &str, port: u16) -> Option<AddrClass> {
    let addrs = match host.parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => resolver.resolve(host, port).await.ok()?,
    };
    let rank = |c: AddrClass| match c {
        AddrClass::Blocked => 0,
        AddrClass::Public => 1,
        AddrClass::Private => 2,
        AddrClass::Loopback => 3,
    };
    addrs.into_iter().map(classify).min_by_key(|c| rank(*c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn classes() {
        for b in [
            "169.254.169.254",
            "169.254.1.1",
            "fe80::1",
            "0.0.0.0",
            "::",
            "224.0.0.1",
            "255.255.255.255",
            "fd00:ec2::254",
            "100.100.100.200",
            "::ffff:169.254.169.254",
            "ff02::1",
        ] {
            assert_eq!(classify(ip(b)), AddrClass::Blocked, "{b}");
        }
        assert_eq!(classify(ip("127.0.0.1")), AddrClass::Loopback);
        assert_eq!(classify(ip("::1")), AddrClass::Loopback);
        for p in [
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "fc00::1",
            "::ffff:10.0.0.1",
        ] {
            assert_eq!(classify(ip(p)), AddrClass::Private, "{p}");
        }
        assert_eq!(classify(ip("93.184.216.34")), AddrClass::Public);
        assert_eq!(classify(ip("2606:4700::1")), AddrClass::Public);
    }

    /// IPv6 forms that carry an IPv4 address are judged as that address
    /// (independent verification, D5).
    #[test]
    fn ipv6_embedded_ipv4_forms() {
        for (s, c) in [
            ("64:ff9b::a9fe:a9fe", AddrClass::Blocked), // NAT64 of 169.254.169.254
            ("64:ff9b::7f00:1", AddrClass::Blocked),    // NAT64 of 127.0.0.1
            ("64:ff9b::a00:1", AddrClass::Blocked),     // NAT64 of 10.0.0.1
            ("64:ff9b::5db8:d822", AddrClass::Public),  // NAT64 of 93.184.216.34
            ("::a9fe:a9fe", AddrClass::Blocked),        // IPv4-compatible
            ("::7f00:1", AddrClass::Blocked),
            ("::ffff:0:a9fe:a9fe", AddrClass::Blocked), // IPv4-translated
            ("::ffff:0:7f00:1", AddrClass::Blocked),
            ("2002:a9fe:a9fe::1", AddrClass::Blocked), // 6to4
            ("2002:7f00:1::1", AddrClass::Blocked),
            ("2002:a00:1::1", AddrClass::Blocked),
            ("2002:5db8:d822::1", AddrClass::Public),
            ("fec0::1", AddrClass::Blocked), // site-local (deprecated)
            ("64:ff9b:1::a9fe:a9fe", AddrClass::Blocked),
            ("2001:0:4136:e378::1", AddrClass::Blocked), // Teredo
            ("::", AddrClass::Blocked),
            ("::1", AddrClass::Loopback),
        ] {
            assert_eq!(classify(ip(s)), c, "{s}");
        }
        for s in [
            "64:ff9b::a9fe:a9fe",
            "64:ff9b::7f00:1",
            "::7f00:1",
            "::ffff:0:a9fe:a9fe",
            "2002:a9fe:a9fe::1",
            "fec0::1",
        ] {
            assert!(!permitted(ip(s), true, true), "{s}");
        }
    }

    #[test]
    fn http_only_for_private_with_the_switch() {
        // T-M1-114.
        assert!(permitted(ip("10.0.0.1"), true, false));
        assert!(permitted(ip("127.0.0.1"), true, false));
        assert!(!permitted(ip("10.0.0.1"), false, false));
        assert!(permitted(ip("10.0.0.1"), false, true));
        assert!(!permitted(ip("93.184.216.34"), false, true));
        assert!(permitted(ip("93.184.216.34"), true, false));
        // T-M1-119: the switch never opens these.
        for b in [
            "169.254.169.254",
            "fe80::1",
            "0.0.0.0",
            "::ffff:169.254.169.254",
        ] {
            assert!(!permitted(ip(b), true, true), "{b}");
            assert!(!permitted(ip(b), false, true), "{b}");
        }
    }

    /// Answers from a script, one per call (the last repeats).
    struct Script(Mutex<Vec<Vec<IpAddr>>>);

    impl Resolver for Script {
        fn resolve<'a>(
            &'a self,
            _: &'a str,
            _: u16,
        ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
            let mut g = self.0.lock().unwrap();
            let v = if g.len() > 1 {
                g.remove(0)
            } else {
                g[0].clone()
            };
            Box::pin(async move { Ok(v) })
        }
    }

    #[tokio::test]
    async fn rebinding_is_checked_on_every_connection() {
        let r = Script(Mutex::new(vec![
            vec![ip("93.184.216.34")],
            vec![ip("127.0.0.1")],
        ]));
        assert_eq!(
            pick(&r, "llm.example", 443, false, false).await,
            Err(Refused::Blocked),
            "http to a public address"
        );
        let r = Script(Mutex::new(vec![
            vec![ip("93.184.216.34")],
            vec![ip("169.254.169.254")],
        ]));
        assert_eq!(
            pick(&r, "llm.example", 443, true, true).await,
            Ok(ip("93.184.216.34"))
        );
        assert_eq!(
            pick(&r, "llm.example", 443, true, true).await,
            Err(Refused::Blocked),
            "the second answer is checked again"
        );
        // One bad address among good ones refuses the name.
        let r = Script(Mutex::new(vec![vec![
            ip("10.0.0.1"),
            ip("169.254.169.254"),
        ]]));
        assert_eq!(
            pick(&r, "llm.example", 443, true, true).await,
            Err(Refused::Blocked)
        );
        // An IPv4-mapped literal is judged and pinned as IPv4.
        let r = Script(Mutex::new(vec![vec![]]));
        assert_eq!(
            pick(&r, "::ffff:10.0.0.1", 443, true, false).await,
            Ok(ip("10.0.0.1"))
        );
        assert_eq!(
            pick(&r, "nothing.example", 443, true, false).await,
            Err(Refused::Unresolved)
        );
    }
}

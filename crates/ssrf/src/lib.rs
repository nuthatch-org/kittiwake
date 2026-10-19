//! Deciding whether a URL a stranger supplied may be fetched.
//!
//! Several routes probe an indexer's own endpoint, and the address of that endpoint comes from the
//! chain, which anyone may write to. Without this, a registered service URL of
//! `http://169.254.169.254/latest/meta-data/` turns our server into the attacker's HTTP client
//! against our own metadata service.
//!
//! Two checks, and the second is the one people forget. The literal host is checked, and then the
//! name is resolved and **every** address it resolves to is checked, because a name that resolves
//! to a private address is the whole DNS-rebinding trick and passes the first check happily.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Is this address one we must never fetch from?
///
/// The ranges are enumerated rather than delegated to `is_global`, which is still unstable, and
/// each carries the reason it is here.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = embedded_v4(v6) {
                return is_private_v4(v4);
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // 64:ff9b:1::/48, local-use NAT64, where the embedded address may sit anywhere.
                || (v6.segments()[..3] == [0x64, 0xff9b, 1])
                // fe80::/10, link-local.
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // fc00::/7, unique local.
                || (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

/// The IPv4 address an IPv6 one carries: mapped (`::ffff:a.b.c.d`), compatible (`::a.b.c.d`) or
/// well-known NAT64 (`64:ff9b::a.b.c.d`). Each is an IPv4 address wearing a hat; judge it as one.
fn embedded_v4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(mapped) = v6.to_ipv4_mapped() {
        return Some(mapped);
    }
    let s = v6.segments();
    let tail = Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
    match s[..6] {
        [0, 0, 0, 0, 0, 0] if !v6.is_loopback() && !v6.is_unspecified() => Some(tail),
        [0x64, 0xff9b, 0, 0, 0, 0] => Some(tail),
        _ => None,
    }
}

fn is_private_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()      // 169.254/16, which is where the cloud metadata service lives
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || a == 0                   // 0.0.0.0/8, "this network"
        || (a == 100 && (64..=127).contains(&b)) // 100.64/10, carrier-grade NAT
        || (a == 198 && (b & 0xfe) == 18) // 198.18/15, benchmarking
        || a >= 224 // multicast and reserved
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NotAUrl,
    /// Only http and https. A `file:` or `gopher:` URL is not a mistake, it is an attempt.
    Scheme(String),
    /// The name itself is one we never fetch.
    Host(String),
    /// The name resolved to an address we never fetch. This is the rebinding case.
    ResolvesToPrivate(String),
    /// The name did not resolve. Fail closed: an unknown answer is not a safe one.
    Unresolvable(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NotAUrl => write!(f, "not a URL"),
            Refusal::Scheme(s) => write!(f, "scheme {s:?} is not fetchable"),
            Refusal::Host(h) => write!(f, "{h} is not an address we will fetch from"),
            Refusal::ResolvesToPrivate(h) => write!(f, "{h} resolves to a private address"),
            Refusal::Unresolvable(h) => write!(f, "{h} did not resolve"),
        }
    }
}

impl std::error::Error for Refusal {}

/// The cheap check: scheme, and the host as written.
///
/// Passing this is not permission to fetch. It only means the URL is not *obviously* pointed
/// inward; [`check_resolved`] is the one that decides.
pub fn check_literal(raw: &str) -> Result<url::Url, Refusal> {
    let u = url::Url::parse(raw).map_err(|_| Refusal::NotAUrl)?;
    match u.scheme() {
        "http" | "https" => {}
        other => return Err(Refusal::Scheme(other.to_string())),
    }
    let host = u.host_str().ok_or(Refusal::NotAUrl)?.to_lowercase();

    if host == "localhost" || host.ends_with(".localhost") {
        return Err(Refusal::Host(host));
    }
    // Strip a zone index (`fe80::1%eth0`) and brackets before parsing.
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let bare = bare.split('%').next().unwrap_or(bare);
    if let Ok(ip) = bare.parse::<IpAddr>()
        && is_private_ip(ip)
    {
        return Err(Refusal::Host(host));
    }
    Ok(u)
}

/// The real check: resolve the name and refuse if *any* address it answers with is private.
///
/// `any` rather than `all` is deliberate. A name that resolves to one public and one private
/// address is not half safe; the connection may go to either.
pub async fn check_resolved(raw: &str) -> Result<url::Url, Refusal> {
    let u = check_literal(raw)?;
    let host = u.host_str().unwrap_or_default().to_string();

    // A literal address needs no lookup and has already been judged.
    if host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .is_ok()
    {
        return Ok(u);
    }

    let port = u.port_or_known_default().unwrap_or(80);
    let target = format!("{host}:{port}");
    let addrs = tokio::net::lookup_host(target)
        .await
        .map_err(|_| Refusal::Unresolvable(host.clone()))?
        .collect::<Vec<_>>();

    if addrs.is_empty() {
        return Err(Refusal::Unresolvable(host));
    }
    if addrs.iter().any(|a| is_private_ip(a.ip())) {
        return Err(Refusal::ResolvesToPrivate(host));
    }
    Ok(u)
}

/// Addresses refused even for a URL the operator wrote into their own config.
///
/// Loopback, RFC 1918 and 100.64/10 stay open because that is where an indexer agent lives
/// (a Tailscale address is 100.64/10). Link-local is the cloud metadata service.
pub fn is_never_a_service(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_never_a_service_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = embedded_v4(v6) {
                return is_never_a_service_v4(v4);
            }
            v6.is_unspecified() || v6.is_multicast() || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

fn is_never_a_service_v4(ip: Ipv4Addr) -> bool {
    ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.octets()[0] == 0
        || ip.octets()[0] >= 224
}

/// The check for a URL that comes from the operator's own configuration rather than from a
/// stranger: private networks allowed, metadata and unroutable addresses refused, every resolved
/// address checked. Returns the addresses so the caller can pin its connections to them.
pub async fn check_configured(raw: &str) -> Result<(url::Url, Vec<std::net::SocketAddr>), Refusal> {
    let u = url::Url::parse(raw).map_err(|_| Refusal::NotAUrl)?;
    match u.scheme() {
        "http" | "https" => {}
        other => return Err(Refusal::Scheme(other.to_string())),
    }
    let host = u.host_str().ok_or(Refusal::NotAUrl)?.to_lowercase();
    let port = u.port_or_known_default().unwrap_or(80);
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<std::net::SocketAddr> = match bare.parse::<IpAddr>() {
        Ok(ip) => vec![std::net::SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host(format!("{host}:{port}"))
            .await
            .map_err(|_| Refusal::Unresolvable(host.clone()))?
            .collect(),
    };
    if addrs.is_empty() {
        return Err(Refusal::Unresolvable(host));
    }
    if let Some(bad) = addrs.iter().find(|a| is_never_a_service(a.ip())) {
        return Err(if bare.parse::<IpAddr>().is_ok() {
            Refusal::Host(host)
        } else {
            Refusal::ResolvesToPrivate(format!("{host} ({})", bad.ip()))
        });
    }
    Ok((u, addrs))
}

/// A client for URLs a stranger supplied. Every name is judged again at connect time, so neither a
/// DNS answer that changes after [`check_resolved`] nor a redirect to a name can reach inward; a
/// literal address skips the resolver, so each redirect hop is checked as written.
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if let Err(refusal) = check_literal(attempt.url().as_str()) {
                attempt.error(refusal)
            } else {
                attempt.follow()
            }
        }))
        .no_proxy()
        .dns_resolver(Guarded)
}

struct Guarded;

impl reqwest::dns::Resolve for Guarded {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|_| Refusal::Unresolvable(host.clone()))?
                .collect();
            if addrs.is_empty() {
                return Err(Refusal::Unresolvable(host).into());
            }
            if addrs.iter().any(|a| is_private_ip(a.ip())) {
                return Err(Refusal::ResolvesToPrivate(host).into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn the_cloud_metadata_address_is_refused() {
        assert!(is_private_ip(ip("169.254.169.254")));
        assert!(check_literal("http://169.254.169.254/latest/meta-data/").is_err());
    }

    #[test]
    fn every_private_range_is_covered() {
        for a in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "172.31.255.255",
            "169.254.1.1",
            "0.0.0.0",
            "100.64.0.1",
            "100.127.255.255",
            "224.0.0.1",
            "255.255.255.255",
        ] {
            assert!(is_private_ip(ip(a)), "{a} should be refused");
        }
        for a in [
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd00::1",
            "::ffff:127.0.0.1",
            "::127.0.0.1",
            "::a9fe:a9fe",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b:1::1",
            "ff02::1",
            "ff05::2",
            "198.18.0.1",
            "198.19.255.255",
        ] {
            assert!(is_private_ip(ip(a)), "{a} should be refused");
        }
    }

    #[test]
    fn public_addresses_are_allowed() {
        for a in [
            "1.1.1.1",
            "8.8.8.8",
            "89.167.109.4",
            "167.235.29.213",
            "2606:4700::1111",
        ] {
            assert!(!is_private_ip(ip(a)), "{a} should be allowed");
        }
        // 172.32 is outside the 172.16/12 private block; an off-by-one here would block real hosts.
        assert!(!is_private_ip(ip("172.32.0.1")));
        assert!(!is_private_ip(ip("100.128.0.1")));
        assert!(!is_private_ip(ip("198.20.0.1")));
        assert!(!is_private_ip(ip("64:ff9b::808:808")));
    }

    #[test]
    fn a_v4_address_wearing_a_v6_hat_is_still_judged_as_v4() {
        assert!(is_private_ip(ip("::ffff:10.0.0.1")));
        assert!(!is_private_ip(ip("::ffff:8.8.8.8")));
    }

    #[test]
    fn only_http_schemes_are_fetchable() {
        assert!(matches!(
            check_literal("file:///etc/passwd"),
            Err(Refusal::Scheme(_))
        ));
        assert!(matches!(
            check_literal("gopher://example.com/"),
            Err(Refusal::Scheme(_))
        ));
        assert!(check_literal("https://indexer.example.com/status").is_ok());
    }

    #[test]
    fn localhost_by_any_name_is_refused() {
        assert!(check_literal("http://localhost:8000/").is_err());
        assert!(check_literal("http://evil.localhost/").is_err());
        assert!(check_literal("http://[::1]:8000/").is_err());
    }

    #[test]
    fn nonsense_is_not_a_url() {
        assert_eq!(check_literal("not a url").unwrap_err(), Refusal::NotAUrl);
        assert_eq!(check_literal("").unwrap_err(), Refusal::NotAUrl);
    }

    /// The check people skip: `localtest.me` and friends are public names that resolve to
    /// loopback, so the literal check passes and the fetch still goes inward.
    #[tokio::test]
    async fn a_configured_url_may_be_private_but_never_metadata() {
        for ok in [
            "http://127.0.0.1:18000/",
            "http://10.0.0.5:18000/",
            "http://192.168.1.20:18000/",
            "http://100.101.102.103:18000/",
            "http://localhost:18000/",
        ] {
            assert!(check_configured(ok).await.is_ok(), "{ok} should be allowed");
        }
        for bad in [
            "http://169.254.169.254/latest/meta-data/",
            "http://[fe80::1]/",
            "http://0.0.0.0:18000/",
            "http://[::ffff:169.254.169.254]/",
            "http://224.0.0.1/",
            "http://[64:ff9b::a9fe:a9fe]/",
            "http://[::169.254.169.254]/",
            "file:///etc/passwd",
            "gopher://10.0.0.5/",
            "not a url",
        ] {
            assert!(
                check_configured(bad).await.is_err(),
                "{bad} should be refused"
            );
        }
    }

    #[tokio::test]
    async fn a_public_name_that_resolves_inward_is_refused() {
        // Resolved through the host's own resolver; a literal address needs no lookup and takes
        // the fast path, which this asserts as well.
        assert!(check_resolved("http://127.0.0.1/").await.is_err());
        assert!(check_resolved("http://10.0.0.1/").await.is_err());
    }

    #[tokio::test]
    async fn a_name_that_does_not_resolve_fails_closed() {
        let r = check_resolved("http://this-name-does-not-exist.invalid/").await;
        assert!(matches!(r, Err(Refusal::Unresolvable(_))), "got {r:?}");
    }

    async fn serve_once(answer: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(answer.as_bytes()).await;
            }
        });
        port
    }

    #[tokio::test]
    async fn a_redirect_inward_is_refused() {
        let inner = serve_once("HTTP/1.1 200 OK\r\ncontent-length: 6\r\n\r\nsecret").await;
        let redirect: &'static str = Box::leak(
            format!(
                "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:{inner}/\r\ncontent-length: 0\r\n\r\n"
            )
            .into_boxed_str(),
        );
        let outer = serve_once(redirect).await;
        let client = client_builder().build().unwrap();
        let res = client
            .get(format!("http://127.0.0.1:{outer}/"))
            .send()
            .await;
        assert!(res.unwrap_err().is_redirect());
    }

    /// The name was public when it was checked; by the time the client connects it says loopback.
    #[tokio::test]
    async fn a_name_that_resolves_inward_at_connect_time_is_refused() {
        let port = serve_once("HTTP/1.1 200 OK\r\ncontent-length: 6\r\n\r\nsecret").await;
        let client = client_builder().build().unwrap();
        let res = client.get(format!("http://localhost:{port}/")).send().await;
        assert!(res.is_err(), "reached loopback through a name: {res:?}");
    }
}

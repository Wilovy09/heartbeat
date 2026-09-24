//! Every request this app makes to a registered app's URL (logs proxy, uptime probes) goes
//! through `Outbound`. Registered URLs are admin-entered data and those requests carry
//! secrets (`ADMIN_LOGS_KEY`, the admin's JWT), so:
//!
//! - only `https://` URLs whose host matches `ALLOWED_HOSTS` are allowed -- secrets can't be
//!   pointed at an arbitrary server or sent in cleartext;
//! - DNS answers are filtered to public addresses, so an allowed name that resolves to
//!   loopback/private/link-local space (incl. the cloud metadata endpoint 169.254.169.254)
//!   is refused instead of turning this server into a proxy into its own network;
//! - redirects are never followed, since a 3xx could otherwise bounce the request (and its
//!   headers) past both checks above.

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::{Client, Url};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum OutboundError {
    #[error("URL inválida: {0}")]
    InvalidUrl(#[from] url::ParseError),
    #[error("solo se permiten URLs https://")]
    NotHttps,
    #[error("el host '{0}' no está en ALLOWED_HOSTS")]
    HostNotAllowed(String),
    #[error("no se pudo crear el cliente HTTP: {0}")]
    Client(#[from] reqwest::Error),
}

/// One `ALLOWED_HOSTS` entry: `api.example.com` (exact) or `*.example.com` (any subdomain,
/// not the bare domain itself).
#[derive(Debug, Clone, PartialEq, Eq)]
enum HostPattern {
    Exact(String),
    Subdomain(String),
}

impl HostPattern {
    fn matches(&self, host: &str) -> bool {
        match self {
            Self::Exact(h) => host == h,
            Self::Subdomain(suffix) => host
                .strip_suffix(suffix.as_str())
                .is_some_and(|label| label.len() > 1 && label.ends_with('.')),
        }
    }
}

impl From<&str> for HostPattern {
    fn from(raw: &str) -> Self {
        let raw = raw.trim().to_ascii_lowercase();
        match raw.strip_prefix("*.") {
            Some(domain) => Self::Subdomain(domain.to_string()),
            None => Self::Exact(raw),
        }
    }
}

/// Resolves normally, then drops every non-public address; errors if none are left.
struct PublicOnlyResolver;

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_string();
            let public: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|addr| is_public(addr.ip()))
                .collect();
            if public.is_empty() {
                return Err(format!("'{host}' no resuelve a ninguna IP pública").into());
            }
            let addrs: Addrs = Box::new(public.into_iter());
            Ok(addrs)
        })
    }
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(v6),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    let shared_cgnat = a == 100 && (64..128).contains(&b); // 100.64.0.0/10
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local() // 169.254.0.0/16, incl. cloud metadata
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || shared_cgnat
        || a == 0)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_unique_local() // fc00::/7
        || ip.is_unicast_link_local()) // fe80::/10
}

/// Shared HTTP client + allowlist for requests to registered apps. Cheap to clone.
#[derive(Clone)]
pub struct Outbound {
    client: Client,
    allowed_hosts: Arc<[HostPattern]>,
}

impl Outbound {
    /// `allowed_hosts`: comma-separated `ALLOWED_HOSTS` value.
    pub fn new(allowed_hosts: &str) -> Result<Self, OutboundError> {
        let client = Client::builder()
            .dns_resolver(Arc::new(PublicOnlyResolver))
            .redirect(reqwest::redirect::Policy::none())
            .timeout(DEFAULT_TIMEOUT)
            .user_agent("heartbeat")
            .build()?;
        let allowed_hosts = allowed_hosts
            .split(',')
            .filter(|h| !h.trim().is_empty())
            .map(HostPattern::from)
            .collect();
        Ok(Self {
            client,
            allowed_hosts,
        })
    }

    /// Parses `raw` and checks it against the policy; the only way to get a URL this
    /// client will send secrets to.
    pub fn check(&self, raw: &str) -> Result<Url, OutboundError> {
        let url = Url::parse(raw.trim())?;
        if url.scheme() != "https" {
            return Err(OutboundError::NotHttps);
        }
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        if !self.allowed_hosts.iter().any(|p| p.matches(&host)) {
            return Err(OutboundError::HostNotAllowed(host));
        }
        Ok(url)
    }

    #[must_use]
    pub fn client(&self) -> &Client {
        &self.client
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_enforces_https_and_the_allowlist() {
        let out = Outbound::new("*.example.com, api.partner.com").unwrap();
        assert!(out.check("https://api.example.com/health").is_ok());
        assert!(out.check("https://a.b.example.com/x").is_ok());
        assert!(out.check("https://API.PARTNER.COM/x").is_ok());

        assert!(matches!(
            out.check("http://api.example.com/x"),
            Err(OutboundError::NotHttps)
        ));
        for bad in [
            "https://example.com/x",          // bare domain isn't a subdomain
            "https://evil-example.com/x",     // suffix without the dot
            "https://example.com.evil.com/x", // allowed name as a prefix
            "https://sub.partner.com/x",      // exact entry doesn't cover subdomains
            "https://169.254.169.254/latest/meta-data/",
            "https://127.0.0.1/x",
        ] {
            assert!(
                matches!(out.check(bad), Err(OutboundError::HostNotAllowed(_))),
                "{bad}"
            );
        }
        assert!(
            Outbound::new("")
                .unwrap()
                .check("https://x.example.com")
                .is_err()
        );
    }

    #[test]
    fn only_public_ips_pass() {
        for blocked in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.5.4",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
        ] {
            assert!(!is_public(blocked.parse().unwrap()), "{blocked}");
        }
        for allowed in ["1.1.1.1", "54.23.10.8", "2606:4700:4700::1111"] {
            assert!(is_public(allowed.parse().unwrap()), "{allowed}");
        }
    }

    #[tokio::test]
    async fn resolver_refuses_names_that_only_resolve_privately() {
        let err = PublicOnlyResolver
            .resolve("localhost".parse().unwrap())
            .await
            .err()
            .expect("localhost must be refused");
        assert!(err.to_string().contains("IP pública"));
    }
}

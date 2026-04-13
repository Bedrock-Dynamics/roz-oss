//! SSRF-guarded HTTPS fetcher for `MediaPart::file_uri` (Phase 16.1 / MED-03).
//!
//! Design:
//! * Only `https://` is accepted (D-13).
//! * Hostname is pre-resolved via `hickory-resolver`; any resolved IP in a
//!   private / loopback / link-local / CGNAT / IPv6-ULA / IPv4-mapped range
//!   is rejected with `FailedPrecondition` (D-15).
//! * The reqwest connection is pinned to the validated IPs via
//!   `ClientBuilder::resolve_to_addrs` — closes the TOCTOU window where a
//!   second DNS lookup inside reqwest could return a different IP.
//! * Redirects are disabled (`redirect::Policy::none`) to prevent SSRF via
//!   redirect hop to a private IP.
//! * Content-Length > 100 MB fails fast. Stream-read enforces the cap with a
//!   running tally; on overflow we abort with `ResourceExhausted`.
//! * 30 s total timeout on the HTTP client.
//! * Response Content-Type family must match the requested mime family
//!   (`video|image|audio`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use futures::StreamExt as _;
use hickory_resolver::TokioAsyncResolver;
use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use tonic::Status;

const MAX_BYTES: u64 = 100 * 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

pub struct MediaFetcher {
    resolver: TokioAsyncResolver,
}

impl Default for MediaFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaFetcher {
    #[must_use]
    pub fn new() -> Self {
        let resolver = TokioAsyncResolver::tokio(ResolverConfig::default(), ResolverOpts::default());
        Self { resolver }
    }

    /// Fetch `uri` into memory, enforcing SSRF / size / timeout / mime guards.
    #[allow(
        clippy::too_many_lines,
        reason = "Single cohesive fetch pipeline: URL parse + DNS + SSRF check + HTTP + stream cap"
    )]
    pub async fn fetch(&self, uri: &str, expected_mime_family: &str) -> Result<Vec<u8>, Status> {
        let url = reqwest::Url::parse(uri).map_err(|e| Status::invalid_argument(format!("invalid file_uri: {e}")))?;
        if url.scheme() != "https" {
            return Err(Status::invalid_argument("file_uri scheme must be https (D-13)"));
        }
        let host = url
            .host_str()
            .ok_or_else(|| Status::invalid_argument("file_uri missing hostname"))?
            .to_string();
        if host.is_empty() {
            return Err(Status::invalid_argument("file_uri missing hostname"));
        }
        let port = url.port_or_known_default().unwrap_or(443);

        let lookup = self
            .resolver
            .lookup_ip(host.as_str())
            .await
            .map_err(|e| Status::unavailable(format!("dns resolution failed: {e}")))?;

        let mut safe_addrs: Vec<SocketAddr> = Vec::new();
        for ip in lookup.iter() {
            if is_blocked_ip(&ip) {
                return Err(Status::failed_precondition(format!(
                    "file_uri resolves to blocked IP: {ip}"
                )));
            }
            safe_addrs.push(SocketAddr::new(ip, port));
        }
        if safe_addrs.is_empty() {
            return Err(Status::failed_precondition(
                "file_uri hostname has no resolvable public IPs",
            ));
        }

        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(host.as_str(), &safe_addrs)
            .build()
            .map_err(|e| Status::internal(format!("reqwest build: {e}")))?;

        let resp = client.get(url).send().await.map_err(|e| {
            if e.is_timeout() {
                Status::deadline_exceeded("file_uri fetch timeout (30s)")
            } else {
                Status::unavailable(format!("file_uri fetch failed: {e}"))
            }
        })?;

        if !resp.status().is_success() {
            return Err(Status::unavailable(format!("file_uri HTTP {}", resp.status())));
        }

        // Content-Type family check (on FINAL response — Pitfall 3).
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let ct_family = ct.split('/').next().unwrap_or("");
        if ct_family != expected_mime_family {
            return Err(Status::invalid_argument(format!(
                "fetched Content-Type '{ct}' does not match expected family '{expected_mime_family}/*'"
            )));
        }

        // Fail-fast on Content-Length over cap (Pitfall 4).
        if let Some(len) = resp.content_length()
            && len > MAX_BYTES
        {
            return Err(Status::resource_exhausted(format!(
                "file_uri body {len} bytes exceeds 100 MB cap"
            )));
        }

        let cap_hint = usize::try_from(resp.content_length().map_or(0, |n| n.min(MAX_BYTES))).unwrap_or(0);
        let mut bytes: Vec<u8> = Vec::with_capacity(cap_hint);
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                if e.is_timeout() {
                    Status::deadline_exceeded("file_uri body timeout")
                } else {
                    Status::unavailable(format!("body read failed: {e}"))
                }
            })?;
            if bytes.len() as u64 + chunk.len() as u64 > MAX_BYTES {
                return Err(Status::resource_exhausted("file_uri body exceeds 100 MB cap"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

/// Reject private / loopback / link-local / CGNAT / IPv6 ULA / IPv4-mapped.
pub(crate) const fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(*v4),
        IpAddr::V6(v6) => is_blocked_v6(*v6),
    }
}

const fn is_blocked_v4(v4: Ipv4Addr) -> bool {
    if v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_unspecified()
        || v4.is_documentation()
    {
        return true;
    }
    let oct = v4.octets();
    // 0.0.0.0/8 "this network"
    if oct[0] == 0 {
        return true;
    }
    // CGNAT 100.64.0.0/10 (RFC 6598)
    if oct[0] == 100 && (oct[1] & 0xC0) == 0x40 {
        return true;
    }
    false
}

const fn is_blocked_v6(v6: Ipv6Addr) -> bool {
    if v6.is_loopback() || v6.is_unspecified() {
        return true;
    }
    if is_ipv6_link_local(v6) || is_ipv6_unique_local(v6) {
        return true;
    }
    if let Some(mapped) = v6.to_ipv4_mapped() {
        return is_blocked_v4(mapped);
    }
    false
}

const fn is_ipv6_link_local(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfe80
}

const fn is_ipv6_unique_local(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse::<Ipv4Addr>().unwrap())
    }
    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse::<Ipv6Addr>().unwrap())
    }

    #[test]
    fn blocks_ipv4_private_ranges() {
        assert!(is_blocked_ip(&v4("10.0.0.1")));
        assert!(is_blocked_ip(&v4("172.16.0.1")));
        assert!(is_blocked_ip(&v4("192.168.1.1")));
    }

    #[test]
    fn blocks_ipv4_loopback_linklocal_unspec_bcast() {
        assert!(is_blocked_ip(&v4("127.0.0.1")));
        assert!(is_blocked_ip(&v4("169.254.1.1")));
        assert!(is_blocked_ip(&v4("0.0.0.0")));
        assert!(is_blocked_ip(&v4("255.255.255.255")));
    }

    #[test]
    fn blocks_ipv4_cgnat() {
        assert!(is_blocked_ip(&v4("100.64.0.1")));
        assert!(is_blocked_ip(&v4("100.127.255.254")));
    }

    #[test]
    fn does_not_block_ipv4_public() {
        assert!(!is_blocked_ip(&v4("8.8.8.8")));
        assert!(!is_blocked_ip(&v4("1.1.1.1")));
        assert!(!is_blocked_ip(&v4("100.63.255.255"))); // just outside CGNAT
        assert!(!is_blocked_ip(&v4("100.128.0.0"))); // just outside CGNAT
    }

    #[test]
    fn blocks_ipv6_loopback_unspec_linklocal_ula() {
        assert!(is_blocked_ip(&v6("::1")));
        assert!(is_blocked_ip(&v6("::")));
        assert!(is_blocked_ip(&v6("fe80::1")));
        assert!(is_blocked_ip(&v6("fc00::1")));
        assert!(is_blocked_ip(&v6("fd00::1")));
    }

    #[test]
    fn blocks_ipv6_mapped_to_private_v4() {
        assert!(is_blocked_ip(&v6("::ffff:10.0.0.1")));
        assert!(is_blocked_ip(&v6("::ffff:127.0.0.1")));
    }

    #[test]
    fn does_not_block_ipv6_public() {
        assert!(!is_blocked_ip(&v6("2001:4860:4860::8888"))); // Google DNS
        assert!(!is_blocked_ip(&v6("2606:4700:4700::1111"))); // Cloudflare DNS
    }

    #[tokio::test]
    async fn rejects_non_https_scheme() {
        let f = MediaFetcher::new();
        let err = f.fetch("http://example.com/x.png", "image").await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("https"));
    }

    #[tokio::test]
    async fn rejects_file_scheme() {
        let f = MediaFetcher::new();
        let err = f.fetch("file:///etc/passwd", "image").await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn rejects_gs_scheme() {
        let f = MediaFetcher::new();
        let err = f.fetch("gs://bucket/obj", "image").await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn rejects_invalid_url() {
        let f = MediaFetcher::new();
        // Malformed URL — reqwest::Url::parse fails outright, surfacing InvalidArgument.
        let err = f.fetch("https://", "image").await.unwrap_err();
        assert!(
            matches!(err.code(), tonic::Code::InvalidArgument),
            "got {:?}",
            err.code()
        );
    }
}

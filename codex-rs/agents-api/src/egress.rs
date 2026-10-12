//! Checks for requests the service makes to caller-named URLs. A URL must be
//! https and resolve only to public addresses, unless the operator allows its
//! host, which may then also be internal or use plain http. Callers connect
//! only to the addresses returned, so a DNS change cannot redirect them.
use axum::http::Uri;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Mutex;

/// Why a URL may not be reached.
pub(crate) enum Refusal {
    /// Not an absolute https URL (or http on an allowed host) of at most 2048
    /// bytes.
    Url,
    /// The host did not resolve.
    Unresolved(String),
    /// The host resolves to an address that is not public.
    NonPublic,
}

/// The host and checked addresses a request to `url` may use.
pub(crate) async fn destination(
    url: &str,
    allowed_hosts: &Mutex<HashSet<String>>,
) -> Result<(String, Vec<SocketAddr>), Refusal> {
    let uri: Uri = url.parse().map_err(|_| Refusal::Url)?;
    let host = uri
        .host()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    let allowed = crate::lock(allowed_hosts).contains(&host);
    let https = uri.scheme_str() == Some("https");
    if host.is_empty()
        || url.len() > 2048
        || !(https || (allowed && uri.scheme_str() == Some("http")))
    {
        return Err(Refusal::Url);
    }
    let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|_| Refusal::Unresolved(host.clone()))?
        .collect();
    if addresses.is_empty()
        || (!allowed
            && !addresses
                .iter()
                .all(|address| crate::mcp::public(&address.ip())))
    {
        return Err(Refusal::NonPublic);
    }
    Ok((host, addresses))
}

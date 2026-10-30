//! Fetches the calendar behind a link someone gave us. Anyone can enter any
//! link, so a link must never reach this server's own network.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::{StatusCode, Url, redirect};

use crate::config::user_agent;
use crate::error::{IcalError, Result};

const MAX_FEED_BYTES: usize = 20 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const NOT_A_CALENDAR: &str = "This link does not lead to a calendar. Check that you copied the \
    whole iCal or webcal link.";
const LINK_RULES: &str = "The link must be a public https or webcal address.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addresses {
    PublicOnly,
    /// For tests, whose stand-in calendar runs on this computer.
    Any,
}

pub struct Feeds {
    http: reqwest::Client,
    addresses: Addresses,
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || a == 0
        || a >= 240
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (18..20).contains(&b)))
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_public_v4(mapped);
    }
    let [first, second, ..] = ip.segments();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
        || (first == 0x2001 && second == 0x0db8)
        || first == 0x0064)
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => is_public_v6(ip),
    }
}

/// Checking when connecting, not only when reading the link, also catches a
/// name that resolves to something else the second time.
struct PublicAddresses;

impl Resolve for PublicAddresses {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let found = tokio::net::lookup_host((name.as_str(), 0)).await?;
            let public: Vec<SocketAddr> = found.filter(|address| is_public(address.ip())).collect();
            if public.is_empty() {
                return Err("the name has no public address".into());
            }
            let addresses: Addrs = Box::new(public.into_iter());
            Ok(addresses)
        })
    }
}

fn allowed_url(url: &Url) -> bool {
    let public_host = match url.host() {
        Some(url::Host::Domain(_)) => true,
        Some(url::Host::Ipv4(ip)) => is_public_v4(ip),
        Some(url::Host::Ipv6(ip)) => is_public_v6(ip),
        None => false,
    };
    url.scheme() == "https"
        && public_host
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(url.port(), None | Some(443))
}

fn client(addresses: Addresses) -> reqwest::Client {
    let builder = reqwest::Client::builder()
        .user_agent(user_agent())
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(5))
        .no_proxy();
    let builder = match addresses {
        Addresses::Any => builder,
        Addresses::PublicOnly => {
            builder
                .dns_resolver(Arc::new(PublicAddresses))
                .redirect(redirect::Policy::custom(|attempt| {
                    if attempt.previous().len() >= MAX_REDIRECTS || !allowed_url(attempt.url()) {
                        attempt.stop()
                    } else {
                        attempt.follow()
                    }
                }))
        }
    };
    builder
        .build()
        .expect("the HTTP client has a valid configuration")
}

fn network(error: reqwest::Error) -> IcalError {
    // The link can carry a token, so the URL is dropped from the message.
    IcalError::new(format!(
        "The calendar could not be reached: {}",
        error.without_url()
    ))
}

impl Feeds {
    pub fn new(addresses: Addresses) -> Self {
        Self {
            http: client(addresses),
            addresses,
        }
    }

    /// The same link is often handed out as webcal://.
    fn read_link(&self, link: &str) -> Result<Url> {
        let link = link.trim().replacen("webcal://", "https://", 1);
        let url = Url::parse(&link).map_err(|_| IcalError::new(LINK_RULES))?;
        match self.addresses {
            Addresses::PublicOnly if !allowed_url(&url) => Err(IcalError::new(LINK_RULES)),
            _ => Ok(url),
        }
    }

    /// Checks that a link leads to a calendar and returns it in its usable form.
    pub async fn check(&self, link: &str) -> Result<Url> {
        let url = self.read_link(link)?;
        self.fetch_url(&url).await?;
        Ok(url)
    }

    pub async fn fetch(&self, link: &str) -> Result<String> {
        self.fetch_url(&self.read_link(link)?).await
    }

    async fn fetch_url(&self, url: &Url) -> Result<String> {
        let mut response = self.http.get(url.clone()).send().await.map_err(network)?;
        if matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ) {
            return Err(IcalError::new(NOT_A_CALENDAR));
        }
        response = response.error_for_status().map_err(network)?;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(network)? {
            if body.len() + chunk.len() > MAX_FEED_BYTES {
                return Err(IcalError::new("This calendar is too large to read."));
            }
            body.extend_from_slice(&chunk);
        }
        let text = String::from_utf8_lossy(&body).into_owned();
        if !text
            .trim_start_matches('\u{feff}')
            .trim_start()
            .starts_with("BEGIN:VCALENDAR")
        {
            return Err(IcalError::new(NOT_A_CALENDAR));
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public_link(link: &str) -> bool {
        Feeds::new(Addresses::PublicOnly).read_link(link).is_ok()
    }

    #[test]
    fn webcal_links_become_https() {
        let feeds = Feeds::new(Addresses::PublicOnly);
        let url = feeds
            .read_link(" webcal://calendar.example/feed?token=abc ")
            .expect("link");
        assert_eq!(url.as_str(), "https://calendar.example/feed?token=abc");
    }

    #[test]
    fn only_public_https_links_are_taken() {
        assert!(public_link("https://calendar.example/feed.ics"));
        assert!(public_link("https://calendar.example:443/feed.ics"));
        for refused in [
            "http://calendar.example/feed.ics",
            "file:///etc/passwd",
            "https://user:secret@calendar.example/feed.ics",
            "https://calendar.example:8080/feed.ics",
            "https://127.0.0.1/feed.ics",
            "https://10.0.0.5/feed.ics",
            "https://169.254.169.254/metadata",
            "https://[::1]/feed.ics",
            "https://[::ffff:10.0.0.5]/feed.ics",
            "https://[fd00::1]/feed.ics",
            "not a link",
        ] {
            assert!(!public_link(refused), "{refused}");
        }
    }

    #[test]
    fn internal_addresses_are_not_public() {
        for address in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "198.18.0.1",
            "240.0.0.1",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "::ffff:192.168.1.1",
            "64:ff9b::a00:1",
        ] {
            let ip: IpAddr = address.parse().expect("an address");
            assert!(!is_public(ip), "{address}");
        }
        for address in ["134.130.1.1", "8.8.8.8", "2a00:1450:4001::1"] {
            let ip: IpAddr = address.parse().expect("an address");
            assert!(is_public(ip), "{address}");
        }
    }

    #[tokio::test]
    async fn a_name_for_this_computer_is_refused_when_connecting() {
        let name: Name = "localhost".parse().expect("a name");
        assert!(PublicAddresses.resolve(name).await.is_err());
    }
}

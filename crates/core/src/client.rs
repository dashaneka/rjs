use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;
use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, HeaderMap, HeaderValue};

use crate::Result;

pub(crate) const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

/// HTTP client shared by extractors and the downloader, so cookies a site sets
/// while looking up a video are sent again when downloading it.
#[derive(Clone)]
pub struct Client {
    pub(crate) http: reqwest::Client,
    jar: Arc<Jar>,
}

pub(crate) struct Page {
    /// Final URL after redirects.
    pub url: Url,
    pub html: String,
}

impl Client {
    pub fn new() -> Result<Self> {
        // Fails only when a provider is already installed, which is just as good.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let jar = Arc::new(Jar::default());
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));

        let http = reqwest::Client::builder()
            .user_agent(BROWSER_UA)
            .default_headers(headers)
            .cookie_provider(jar.clone())
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { http, jar })
    }

    /// Loads a web page the way a browser navigating to it would.
    pub(crate) async fn page(&self, url: &str) -> Result<Page> {
        let resp = self
            .http
            .get(url)
            .header(
                ACCEPT,
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Sec-Fetch-Dest", "document")
            .header("Sec-Fetch-Mode", "navigate")
            .header("Sec-Fetch-Site", "none")
            .header("Upgrade-Insecure-Requests", "1")
            .send()
            .await?;
        Ok(Page {
            url: resp.url().clone(),
            html: resp.text().await?,
        })
    }

    /// Sends these cookies (e.g. a logged-in session) with every matching request.
    /// Returns how many were added; cookies for other sites are ignored.
    pub fn add_cookies(&self, cookies: &[crate::Cookie]) -> usize {
        let mut added = 0;
        for c in cookies
            .iter()
            .filter(|c| crate::cookies::for_supported_site(&c.domain))
        {
            let host = c.domain.trim_start_matches('.');
            let path = if c.path.starts_with('/') {
                c.path.as_str()
            } else {
                "/"
            };
            let Ok(url) = Url::parse(&format!("https://{host}{path}")) else {
                continue;
            };
            let mut header = format!("{}={}; Path={path}", c.name, c.value);
            if c.include_subdomains {
                header += &format!("; Domain={host}");
            }
            if c.secure {
                header += "; Secure";
            }
            self.jar.add_cookie_str(&header, &url);
            added += 1;
        }
        added
    }

    /// Stores a cookie as if `url` had sent it in a `Set-Cookie` header.
    pub(crate) fn set_cookie(&self, set_cookie: &str, url: &Url) {
        self.jar.add_cookie_str(set_cookie, url);
    }

    pub(crate) fn cookie(&self, url: &Url, name: &str) -> Option<String> {
        let header = self.jar.cookies(url)?;
        header
            .to_str()
            .ok()?
            .split("; ")
            .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
            .map(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cookie;

    fn cookie(domain: &str, name: &str) -> Cookie {
        Cookie {
            domain: domain.into(),
            include_subdomains: domain.starts_with('.'),
            path: "/".into(),
            secure: true,
            name: name.into(),
            value: "v".into(),
        }
    }

    #[test]
    fn added_cookies_reach_matching_sites_only() {
        let client = Client::new().unwrap();
        let added = client.add_cookies(&[
            cookie(".instagram.com", "sessionid"),
            cookie("www.tiktok.com", "tt"),
            cookie(".example.com", "other"),
        ]);
        assert_eq!(added, 2);

        let url = |s| Url::parse(s).unwrap();
        assert!(
            client
                .cookie(&url("https://i.instagram.com/api/"), "sessionid")
                .is_some()
        );
        assert!(
            client
                .cookie(&url("https://www.tiktok.com/@a"), "tt")
                .is_some()
        );
        assert!(client.cookie(&url("https://m.tiktok.com/"), "tt").is_none());
    }
}

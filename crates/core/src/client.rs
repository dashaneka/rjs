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
    /// Same as `http` but never says which site it's connecting to; see [`hidden_name`].
    hidden: reqwest::Client,
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

        let builder = || {
            reqwest::Client::builder()
                .user_agent(BROWSER_UA)
                .default_headers(headers.clone())
                .cookie_provider(jar.clone())
                .connect_timeout(Duration::from_secs(15))
                .read_timeout(Duration::from_secs(30))
        };
        let http = builder().build()?;
        let hidden = builder()
            .tls_backend_preconfigured(hidden_name::tls_config()?)
            .build()?;
        Ok(Self { http, hidden, jar })
    }

    /// The client to reach `url` with: sites that networks block by name get one
    /// that doesn't send it.
    pub(crate) fn http_for(&self, url: &str) -> &reqwest::Client {
        let host = Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned));
        match host {
            Some(host) if hidden_name::wanted(&host) => &self.hidden,
            _ => &self.http,
        }
    }

    /// Loads a web page the way a browser navigating to it would.
    pub(crate) async fn page(&self, url: &str) -> Result<Page> {
        let resp = self
            .http_for(url)
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

/// Some networks block sites by reading the name a client sends in plain text
/// at the start of a TLS connection. Leaving it out gets through, and the server
/// then shows its operator's default certificate instead of the site's.
mod hidden_name {
    use std::sync::Arc;

    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{CertificateError, DigitallySignedStruct, Error, SignatureScheme};
    use sha2::{Digest, Sha256};

    /// Sites (and their video servers) that get through only without a name.
    const HOSTS: &[&str] = &[
        "xvideos.com",
        "xvideos2.com",
        "xvideos.es",
        "xvideos-cdn.com",
        "xnxx.com",
        "xnxx-cdn.com",
    ];

    /// Default certificates those servers are trusted to show instead.
    const OPERATOR_NAMES: &[&str] = &["www.cdn77.com"];

    /// SHA-256 of the site's own default certificate (`fallback.wgcz.net`, valid
    /// until 2036), which is self-signed and so can only be trusted by pinning.
    const PINNED: &str = "2fdbe574b58920d423681deecbb69e988c23aa6308f23904f34bfcd8037232a2";

    pub(super) fn wanted(host: &str) -> bool {
        HOSTS
            .iter()
            .any(|d| host == *d || host.strip_suffix(d).is_some_and(|s| s.ends_with('.')))
    }

    pub(super) fn tls_config() -> crate::Result<rustls::ClientConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = rustls_platform_verifier::Verifier::new(provider.clone())?;
        let mut config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(OperatorNames(verifier)))
            .with_no_client_auth();
        config.enable_sni = false;
        Ok(config)
    }

    fn is_pinned(cert: &CertificateDer<'_>) -> bool {
        let digest = Sha256::digest(cert.as_ref());
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
            == PINNED
    }

    /// Checks certificates as usual, but also accepts the pinned one or one made
    /// out to a known operator.
    #[derive(Debug)]
    struct OperatorNames(rustls_platform_verifier::Verifier);

    impl ServerCertVerifier for OperatorNames {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            intermediates: &[CertificateDer<'_>],
            server_name: &ServerName<'_>,
            ocsp_response: &[u8],
            now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            let verify = |name: &ServerName<'_>| {
                self.0
                    .verify_server_cert(end_entity, intermediates, name, ocsp_response, now)
            };
            match verify(server_name) {
                Err(Error::InvalidCertificate(_)) if is_pinned(end_entity) => {
                    Ok(ServerCertVerified::assertion())
                }
                Err(Error::InvalidCertificate(
                    CertificateError::NotValidForName
                    | CertificateError::NotValidForNameContext { .. },
                )) => OPERATOR_NAMES
                    .iter()
                    .filter_map(|n| ServerName::try_from(*n).ok())
                    .find_map(|n| verify(&n).ok())
                    .ok_or(Error::InvalidCertificate(CertificateError::NotValidForName)),
                result => result,
            }
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            self.0.verify_tls12_signature(message, cert, dss)
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            self.0.verify_tls13_signature(message, cert, dss)
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.supported_verify_schemes()
        }
    }

    #[test]
    fn hides_only_listed_sites() {
        assert!(wanted("www.xvideos.com"));
        assert!(wanted("mp4-cdn77.xvideos-cdn.com"));
        assert!(wanted("xnxx.com"));
        assert!(!wanted("notxnxx.com"));
        assert!(!wanted("www.pornhub.com"));
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

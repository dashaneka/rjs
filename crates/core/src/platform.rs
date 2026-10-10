use std::fmt;

use reqwest::Url;
use serde::Serialize;

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    X,
    TikTok,
    Instagram,
    Facebook,
    Pinterest,
    Pornhub,
    YouPorn,
    RedTube,
    Eporner,
    XVideos,
    Xnxx,
    XHamster,
    SpankBang,
}

/// Domains of each site, including mirrors that serve the same pages.
const DOMAINS: [(Platform, &[&str]); 13] = [
    (Platform::X, &["x.com", "twitter.com"]),
    (Platform::TikTok, &["tiktok.com"]),
    (Platform::Instagram, &["instagram.com", "instagr.am"]),
    (Platform::Facebook, &["facebook.com", "fb.watch", "fb.com"]),
    (
        Platform::Pinterest,
        &[
            "pinterest.com",
            "pin.it",
            "pinterest.at",
            "pinterest.ca",
            "pinterest.ch",
            "pinterest.cl",
            "pinterest.co.kr",
            "pinterest.co.uk",
            "pinterest.com.au",
            "pinterest.com.mx",
            "pinterest.de",
            "pinterest.dk",
            "pinterest.es",
            "pinterest.fr",
            "pinterest.ie",
            "pinterest.it",
            "pinterest.jp",
            "pinterest.nl",
            "pinterest.nz",
            "pinterest.ph",
            "pinterest.pt",
            "pinterest.ru",
            "pinterest.se",
        ],
    ),
    (
        Platform::Pornhub,
        &["pornhub.com", "pornhub.org", "pornhub.net"],
    ),
    (Platform::YouPorn, &["youporn.com"]),
    (Platform::RedTube, &["redtube.com"]),
    (Platform::Eporner, &["eporner.com"]),
    (
        Platform::XVideos,
        &["xvideos.com", "xvideos2.com", "xvideos.es"],
    ),
    (Platform::Xnxx, &["xnxx.com"]),
    (
        Platform::XHamster,
        &[
            "xhamster.com",
            "xhamster.desi",
            "xhamster2.com",
            "xhamster3.com",
            "xhday.com",
            "xhvid.com",
        ],
    ),
    (Platform::SpankBang, &["spankbang.com"]),
];

impl Platform {
    pub fn name(self) -> &'static str {
        match self {
            Self::X => "X",
            Self::TikTok => "TikTok",
            Self::Instagram => "Instagram",
            Self::Facebook => "Facebook",
            Self::Pinterest => "Pinterest",
            Self::Pornhub => "Pornhub",
            Self::YouPorn => "YouPorn",
            Self::RedTube => "RedTube",
            Self::Eporner => "Eporner",
            Self::XVideos => "XVideos",
            Self::Xnxx => "XNXX",
            Self::XHamster => "xHamster",
            Self::SpankBang => "SpankBang",
        }
    }

    pub fn detect(url: &Url) -> Option<Self> {
        let host = url.host_str()?.to_ascii_lowercase();
        let on = |domain: &&str| host == *domain || host.ends_with(&format!(".{domain}"));
        DOMAINS
            .iter()
            .find(|(_, domains)| domains.iter().any(on))
            .map(|(platform, _)| *platform)
    }

    /// Every domain rjs downloads from, for picking out login cookies.
    pub(crate) fn all_domains() -> impl Iterator<Item = &'static str> {
        DOMAINS
            .iter()
            .flat_map(|(_, domains)| domains.iter().copied())
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Parses user input as a URL, accepting links pasted without `https://`.
pub(crate) fn parse_url(input: &str) -> Result<Url> {
    let input = input.trim();
    let full = if input.contains("://") {
        input.to_owned()
    } else {
        format!("https://{input}")
    };
    Url::parse(&full).map_err(|_| Error::Unsupported(input.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(s: &str) -> Option<Platform> {
        Platform::detect(&parse_url(s).unwrap())
    }

    #[test]
    fn detects_platforms() {
        assert_eq!(detect("https://x.com/a/status/1"), Some(Platform::X));
        assert_eq!(detect("https://pin.it/abc"), Some(Platform::Pinterest));
        assert_eq!(
            detect("https://in.pinterest.com/pin/1/"),
            Some(Platform::Pinterest)
        );
        assert_eq!(detect("mobile.twitter.com/a/status/1"), Some(Platform::X));
        assert_eq!(
            detect("https://vm.tiktok.com/ZMabc/"),
            Some(Platform::TikTok)
        );
        assert_eq!(
            detect("https://www.instagram.com/reel/abc/"),
            Some(Platform::Instagram)
        );
        assert_eq!(detect("https://fb.watch/abc/"), Some(Platform::Facebook));
        assert_eq!(
            detect("https://m.facebook.com/watch/?v=1"),
            Some(Platform::Facebook)
        );
        assert_eq!(
            detect("https://www.pornhub.com/view_video.php?viewkey=a"),
            Some(Platform::Pornhub)
        );
        assert_eq!(
            detect("https://de.xhamster.com/videos/a-xh1"),
            Some(Platform::XHamster)
        );
        assert_eq!(
            detect("https://www.xnxx.com/video-a/b"),
            Some(Platform::Xnxx)
        );
        assert_eq!(detect("https://notx.com/a/status/1"), None);
        assert_eq!(detect("https://youtube.com/watch?v=1"), None);
    }
}

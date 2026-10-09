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
}

impl Platform {
    pub fn name(self) -> &'static str {
        match self {
            Self::X => "X",
            Self::TikTok => "TikTok",
            Self::Instagram => "Instagram",
            Self::Facebook => "Facebook",
        }
    }

    pub fn detect(url: &Url) -> Option<Self> {
        let host = url.host_str()?.to_ascii_lowercase();
        let on = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));

        if on("x.com") || on("twitter.com") {
            Some(Self::X)
        } else if on("tiktok.com") {
            Some(Self::TikTok)
        } else if on("instagram.com") || on("instagr.am") {
            Some(Self::Instagram)
        } else if on("facebook.com") || on("fb.watch") || on("fb.com") {
            Some(Self::Facebook)
        } else {
            None
        }
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
        assert_eq!(detect("https://notx.com/a/status/1"), None);
        assert_eq!(detect("https://youtube.com/watch?v=1"), None);
    }
}

use serde::Serialize;

use crate::error::fail;
use crate::{Platform, Result};

/// One video or image found in a post. Posts with several produce several `Media`.
#[derive(Debug, Clone, Serialize)]
pub struct Media {
    pub platform: Platform,
    pub kind: MediaKind,
    pub id: String,
    pub title: Option<String>,
    pub author: Option<String>,
    pub thumbnail: Option<String>,
    /// Length in seconds.
    pub duration: Option<f64>,
    /// When two formats have no size or bitrate to tell them apart, the later one is preferred.
    pub formats: Vec<Format>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Video,
    Image,
}

impl MediaKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Image => "image",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Format {
    pub id: String,
    pub url: String,
    pub ext: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Bits per second.
    pub bitrate: Option<u64>,
    /// Bytes.
    pub size: Option<u64>,
    pub has_video: bool,
    pub has_audio: bool,
    /// Video codec as reported by the site, when it says.
    pub codec: Option<String>,
    pub protocol: Protocol,
    /// Extra request headers the site requires when downloading this format.
    #[serde(skip)]
    pub headers: Vec<(String, String)>,
}

/// How a format is fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// One file.
    Http,
    /// An HLS playlist of short pieces (`.m3u8`).
    Hls,
}

/// What to download for a `Media`.
#[derive(Debug, Clone, Copy)]
pub enum Selection<'a> {
    Single(&'a Format),
    /// Separate video and audio streams that must be joined with ffmpeg.
    Merge {
        video: &'a Format,
        audio: &'a Format,
    },
}

impl Format {
    /// An MP4 with both video and audio.
    pub(crate) fn mp4(id: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            url: url.into(),
            ext: "mp4".into(),
            width: None,
            height: None,
            bitrate: None,
            size: None,
            has_video: true,
            has_audio: true,
            codec: None,
            protocol: Protocol::Http,
            headers: Vec::new(),
        }
    }

    /// An HLS playlist for one quality, with video and audio.
    pub(crate) fn hls(id: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            protocol: Protocol::Hls,
            ..Self::mp4(id, url)
        }
    }

    /// A picture; the file extension is taken from the URL.
    pub(crate) fn image(id: impl Into<String>, url: impl Into<String>) -> Self {
        let url = url.into();
        let path = url
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let ext = ["webp", "png", "gif", "jpeg", "heic"]
            .into_iter()
            .find(|ext| path.ends_with(&format!(".{ext}")) || path.contains(&format!(".{ext}~")))
            .unwrap_or("jpg");
        Self {
            ext: ext.into(),
            has_video: false,
            has_audio: false,
            ..Self::mp4(id, url)
        }
    }

    /// H.265/HEVC files don't open in every player (Windows needs an extra codec),
    /// so they only win when nothing else is available.
    fn plays_everywhere(&self) -> bool {
        let codec = self
            .codec
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        !["265", "hevc", "hvc1", "bytevc1"]
            .iter()
            .any(|c| codec.contains(c))
    }

    fn short_side(&self) -> Option<u32> {
        match (self.width, self.height) {
            (Some(w), Some(h)) => Some(w.min(h)),
            (_, h) => h,
        }
    }

    /// Some sites only say the height, so a lone side still ranks qualities.
    fn pixels(&self) -> u64 {
        match (self.width, self.height) {
            (Some(w), Some(h)) => u64::from(w) * u64::from(h),
            (Some(side), None) | (None, Some(side)) => u64::from(side) * u64::from(side),
            (None, None) => 0,
        }
    }

    /// At the same picture size, a single file beats HLS because it can resume.
    fn rank(&self) -> (bool, u64, bool, u64) {
        (
            self.plays_everywhere(),
            self.pixels(),
            self.protocol == Protocol::Http,
            self.bitrate.unwrap_or(0),
        )
    }
}

impl Media {
    /// Picks the highest quality, preferring files that already contain both video and
    /// audio unless separate streams give a sharper picture.
    pub fn best(&self) -> Option<Selection<'_>> {
        let best = |keep: fn(&Format) -> bool| {
            self.formats
                .iter()
                .filter(|f| keep(f))
                .max_by_key(|f| f.rank())
        };
        let picture = |f: &Format| (f.plays_everywhere(), f.pixels());

        match (
            best(|f| f.has_video && f.has_audio),
            best(|f| f.has_video && !f.has_audio),
            best(|f| f.has_audio && !f.has_video),
        ) {
            (Some(both), Some(video), Some(audio)) if picture(video) > picture(both) => {
                Some(Selection::Merge { video, audio })
            }
            (Some(both), ..) => Some(Selection::Single(both)),
            (None, Some(video), Some(audio)) => Some(Selection::Merge { video, audio }),
            _ => best(|_| true).map(Selection::Single),
        }
    }

    /// Picks `wanted` (see [`Media::choose`]), or the best format when `None`.
    pub fn select(&self, wanted: Option<&str>) -> Result<Selection<'_>> {
        match wanted {
            // Quality choices only apply to videos; pictures always come in their best size.
            Some(wanted) if self.kind == MediaKind::Video => self.choose(wanted).ok_or_else(|| {
                let ids: Vec<&str> = self.formats.iter().map(|f| f.id.as_str()).collect();
                fail(
                    self.platform,
                    format!(
                        "no format matches \"{wanted}\"; available: {}",
                        ids.join(", ")
                    ),
                )
            }),
            _ => self
                .best()
                .ok_or_else(|| fail(self.platform, "nothing downloadable found")),
        }
    }

    /// Picks a format by its ID (e.g. `play`, `hd`), or by quality: `720` or `720p`
    /// matches the video's shorter side, so it works for portrait videos too.
    pub fn choose(&self, wanted: &str) -> Option<Selection<'_>> {
        let wanted = wanted.trim();
        if wanted.eq_ignore_ascii_case("best") {
            return self.best();
        }
        if let Some(f) = self
            .formats
            .iter()
            .find(|f| f.id.eq_ignore_ascii_case(wanted))
        {
            return Some(self.with_audio(f));
        }

        let quality: u32 = wanted.trim_end_matches(['p', 'P']).parse().ok()?;
        let f = self
            .formats
            .iter()
            .filter(|f| f.has_video && f.short_side() == Some(quality))
            .max_by_key(|f| (f.has_audio, f.rank()))?;
        Some(self.with_audio(f))
    }

    /// Adds the best audio stream to a video-only format.
    fn with_audio<'a>(&'a self, f: &'a Format) -> Selection<'a> {
        if f.has_audio || !f.has_video {
            return Selection::Single(f);
        }
        let audio = self
            .formats
            .iter()
            .filter(|a| a.has_audio && !a.has_video)
            .max_by_key(|a| a.rank());
        match audio {
            Some(audio) => Selection::Merge { video: f, audio },
            None => Selection::Single(f),
        }
    }

    pub fn file_name(&self, ext: &str) -> String {
        let stem = match &self.author {
            Some(author) => format!("{author}_{}", self.id),
            None => format!("{}_{}", self.platform.name().to_lowercase(), self.id),
        };
        let clean: String = stem
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || "-_.".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .take(100)
            .collect();
        format!("{clean}.{ext}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(formats: Vec<Format>) -> Media {
        Media {
            platform: Platform::X,
            kind: MediaKind::Video,
            id: "1".into(),
            title: None,
            author: Some("some user/name".into()),
            thumbnail: None,
            duration: None,
            formats,
        }
    }

    fn sized(id: &str, w: u32, h: u32) -> Format {
        Format {
            width: Some(w),
            height: Some(h),
            ..Format::mp4(id, id)
        }
    }

    #[test]
    fn picks_largest_muxed_format() {
        let m = media(vec![
            sized("a", 640, 360),
            sized("b", 1280, 720),
            sized("c", 320, 180),
        ]);
        assert!(matches!(m.best(), Some(Selection::Single(f)) if f.id == "b"));
    }

    #[test]
    fn prefers_h264_over_higher_resolution_hevc() {
        let hevc = Format {
            codec: Some("h265_hvc1".into()),
            ..sized("hevc", 720, 1280)
        };
        let h264 = Format {
            codec: Some("h264".into()),
            ..sized("h264", 576, 1024)
        };
        let m = media(vec![hevc, h264]);
        assert!(matches!(m.best(), Some(Selection::Single(f)) if f.id == "h264"));
    }

    #[test]
    fn chooses_by_id_or_quality() {
        let hevc = Format {
            codec: Some("h265_hvc1".into()),
            ..sized("adapt_lower_720_1", 720, 1280)
        };
        let h264 = Format {
            codec: Some("h264".into()),
            ..sized("play", 576, 1024)
        };
        let m = media(vec![hevc, h264]);
        let id = |s| match m.choose(s) {
            Some(Selection::Single(f)) => Some(f.id.clone()),
            _ => None,
        };
        assert_eq!(id("PLAY").as_deref(), Some("play"));
        assert_eq!(id("720").as_deref(), Some("adapt_lower_720_1"));
        assert_eq!(id("576p").as_deref(), Some("play"));
        assert_eq!(id("best").as_deref(), Some("play"));
        assert_eq!(id("1080"), None);
        assert_eq!(id("nope"), None);
    }

    #[test]
    fn later_format_wins_ties() {
        let m = media(vec![Format::mp4("sd", "x"), Format::mp4("hd", "y")]);
        assert!(matches!(m.best(), Some(Selection::Single(f)) if f.id == "hd"));
    }

    #[test]
    fn merges_separate_streams() {
        let video = Format {
            has_audio: false,
            ..sized("v", 1920, 1080)
        };
        let audio = Format {
            has_video: false,
            ..Format::mp4("a", "a")
        };
        let m = media(vec![video, audio]);
        assert!(
            matches!(m.best(), Some(Selection::Merge { video, audio }) if video.id == "v" && audio.id == "a")
        );
    }

    #[test]
    fn merges_only_when_sharper_than_muxed() {
        let stream = |id, w, h| Format {
            has_audio: false,
            ..sized(id, w, h)
        };
        let audio = Format {
            has_video: false,
            ..Format::mp4("a", "a")
        };
        let m = media(vec![
            sized("muxed", 960, 720),
            stream("v", 1440, 1080),
            audio.clone(),
        ]);
        assert!(matches!(m.best(), Some(Selection::Merge { video, .. }) if video.id == "v"));

        let m = media(vec![
            sized("muxed", 1440, 1080),
            stream("v", 1440, 1080),
            audio,
        ]);
        assert!(matches!(m.best(), Some(Selection::Single(f)) if f.id == "muxed"));
    }

    #[test]
    fn image_extension_comes_from_url() {
        let ext = |url| Format::image("i", url).ext;
        assert_eq!(
            ext("https://p16.tiktokcdn.com/a~tplv-crop:1080:1080.jpeg?x=1"),
            "jpeg"
        );
        assert_eq!(
            ext("https://p16.tiktokcdn.com/a~tplv-crop.webp?x=1"),
            "webp"
        );
        assert_eq!(
            ext("https://pbs.twimg.com/media/abc?format=jpg&name=orig"),
            "jpg"
        );
    }

    #[test]
    fn file_name_is_safe() {
        assert_eq!(media(vec![]).file_name("mp4"), "some_user_name_1.mp4");
    }
}

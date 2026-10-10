use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::future::try_join_all;
use reqwest::StatusCode;
use reqwest::header::{ACCEPT_ENCODING, CONTENT_RANGE, HeaderMap, HeaderName, HeaderValue, RANGE};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};

use crate::{Client, Error, Format, Media, Protocol, Result, Selection};

pub(crate) mod hls;

/// Pieces smaller than this aren't worth a separate connection.
const MIN_PIECE: u64 = 1 << 20;
const RETRIES: u32 = 5;

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    /// Parallel connections per file.
    pub connections: usize,
    /// Format ID or quality (see [`Media::choose`]); `None` picks the best.
    pub format: Option<String>,
    /// Leave files that already exist alone instead of downloading them again.
    pub skip_existing: bool,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            connections: 8,
            format: None,
            skip_existing: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Saved {
    pub path: PathBuf,
    /// The file was already there and wasn't downloaded again.
    pub skipped: bool,
}

/// Receives download progress. Called from several pieces at once.
pub trait Progress: Send + Sync {
    /// A new file starts; `already` bytes were kept from an earlier, interrupted run.
    fn start(&self, _label: &str, _total: Option<u64>, _already: u64) {}
    fn advance(&self, _bytes: u64) {}
}

pub struct NoProgress;
impl Progress for NoProgress {}

/// Downloads the chosen (or best) format of `media` into `dir`.
pub async fn download_media(
    client: &Client,
    media: &Media,
    dir: &Path,
    options: &DownloadOptions,
    progress: &dyn Progress,
) -> Result<Saved> {
    let selection = media.select(options.format.as_deref())?;
    let dest = dir.join(media.file_name(match selection {
        Selection::Single(format) => &format.ext,
        Selection::Merge { .. } => "mp4",
    }));
    if options.skip_existing {
        // HLS downloads end up as `.ts` files when ffmpeg isn't installed.
        for existing in [dest.clone(), dest.with_extension("ts")] {
            if fs::try_exists(&existing).await? {
                return Ok(Saved {
                    path: existing,
                    skipped: true,
                });
            }
        }
    }

    let path = match selection {
        Selection::Single(format) => {
            let label = media.kind.name();
            download_file(client, format, &dest, label, options, progress).await?
        }
        Selection::Merge { video, audio } => {
            let video_path = dest.with_extension(format!("video.{}", video.ext));
            let audio_path = dest.with_extension(format!("audio.{}", audio.ext));
            let video_path =
                download_file(client, video, &video_path, "video", options, progress).await?;
            let audio_path =
                download_file(client, audio, &audio_path, "audio", options, progress).await?;
            merge(&video_path, &audio_path, &dest).await?;
            fs::remove_file(&video_path).await?;
            fs::remove_file(&audio_path).await?;
            dest
        }
    };
    Ok(Saved {
        path,
        skipped: false,
    })
}

/// Downloads one format and returns where it was saved (normally `dest`).
async fn download_file(
    client: &Client,
    format: &Format,
    dest: &Path,
    label: &str,
    options: &DownloadOptions,
    progress: &dyn Progress,
) -> Result<PathBuf> {
    if format.protocol == Protocol::Hls {
        return hls::download(client, format, dest, label, options, progress).await;
    }
    let headers = request_headers(format);

    let probe = client
        .http_for(&format.url)
        .get(&format.url)
        .headers(headers.clone())
        .header(RANGE, "bytes=0-0")
        .send()
        .await?
        .error_for_status()?;
    let total = if probe.status() == StatusCode::PARTIAL_CONTENT {
        total_from_content_range(probe.headers())
    } else {
        None
    };
    drop(probe);

    match total {
        Some(total) => {
            download_pieces(
                client,
                &format.url,
                &headers,
                dest,
                total,
                label,
                options,
                progress,
            )
            .await?;
        }
        None => download_whole(client, &format.url, &headers, dest, label, progress).await?,
    }
    Ok(dest.to_path_buf())
}

/// Splits the file into ranges, downloads them in parallel into `.part` files,
/// then joins them. Existing `.part` files from an interrupted run are resumed.
#[allow(clippy::too_many_arguments)]
async fn download_pieces(
    client: &Client,
    url: &str,
    headers: &HeaderMap,
    dest: &Path,
    total: u64,
    label: &str,
    options: &DownloadOptions,
    progress: &dyn Progress,
) -> Result<()> {
    let count = (total / MIN_PIECE).clamp(1, options.connections.max(1) as u64);
    let piece = total.div_ceil(count);
    let pieces: Vec<(PathBuf, u64, u64)> = (0..count)
        .map(|i| {
            let start = i * piece;
            let end = (start + piece).min(total) - 1;
            (part_path(dest, &format!("part{i}of{count}")), start, end)
        })
        .collect();

    let mut already = 0;
    for (path, start, end) in &pieces {
        already += file_len(path).await.min(end - start + 1);
    }
    progress.start(label, Some(total), already);

    try_join_all(
        pieces.iter().map(|(path, start, end)| {
            fetch_range(client, url, headers, path, *start, *end, progress)
        }),
    )
    .await?;

    let (first, _, _) = &pieces[0];
    let joined = part_path(dest, "part");
    fs::rename(first, &joined).await?;
    let mut out = OpenOptions::new().append(true).open(&joined).await?;
    for (path, _, _) in &pieces[1..] {
        tokio::io::copy(&mut File::open(path).await?, &mut out).await?;
        fs::remove_file(path).await?;
    }
    out.flush().await?;
    fs::rename(&joined, dest).await?;
    Ok(())
}

/// Downloads bytes `start..=end` into `path`, resuming after network errors.
async fn fetch_range(
    client: &Client,
    url: &str,
    headers: &HeaderMap,
    path: &Path,
    start: u64,
    end: u64,
    progress: &dyn Progress,
) -> Result<()> {
    let want = end - start + 1;
    let mut failures = 0;
    loop {
        let have = file_len(path).await;
        if have > want {
            OpenOptions::new()
                .write(true)
                .open(path)
                .await?
                .set_len(want)
                .await?;
        }
        if have >= want {
            return Ok(());
        }

        let result = stream_range(client, url, headers, path, start + have, end, progress).await;
        if file_len(path).await > have {
            failures = 0;
            continue;
        }
        failures += 1;
        if failures > RETRIES {
            return result.and(Err(Error::Io(std::io::Error::other(
                "server stopped sending data before the download finished",
            ))));
        }
        tokio::time::sleep(Duration::from_millis(500 * u64::from(failures))).await;
    }
}

async fn stream_range(
    client: &Client,
    url: &str,
    headers: &HeaderMap,
    path: &Path,
    start: u64,
    end: u64,
    progress: &dyn Progress,
) -> Result<()> {
    let mut resp = client
        .http_for(url)
        .get(url)
        .headers(headers.clone())
        .header(RANGE, format!("bytes={start}-{end}"))
        .send()
        .await?
        .error_for_status()?;
    if resp.status() != StatusCode::PARTIAL_CONTENT {
        return Err(Error::Io(std::io::Error::other(
            "server ignored the requested byte range",
        )));
    }

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    let mut out = BufWriter::with_capacity(256 * 1024, file);
    let result = async {
        while let Some(chunk) = resp.chunk().await? {
            out.write_all(&chunk).await?;
            progress.advance(chunk.len() as u64);
        }
        Ok(())
    }
    .await;
    // Keep whatever arrived before an error so the next attempt resumes from there.
    out.flush().await?;
    result
}

/// For servers that don't support byte ranges: one connection, no resume.
async fn download_whole(
    client: &Client,
    url: &str,
    headers: &HeaderMap,
    dest: &Path,
    label: &str,
    progress: &dyn Progress,
) -> Result<()> {
    let mut resp = client
        .http_for(url)
        .get(url)
        .headers(headers.clone())
        .send()
        .await?
        .error_for_status()?;
    progress.start(label, resp.content_length(), 0);

    let part = part_path(dest, "part");
    let mut out = BufWriter::with_capacity(256 * 1024, File::create(&part).await?);
    while let Some(chunk) = resp.chunk().await? {
        out.write_all(&chunk).await?;
        progress.advance(chunk.len() as u64);
    }
    out.flush().await?;
    fs::rename(&part, dest).await?;
    Ok(())
}

async fn merge(video: &Path, audio: &Path, dest: &Path) -> Result<()> {
    if run_ffmpeg(&[video, audio], &[], dest, "joining video and audio").await? {
        Ok(())
    } else {
        Err(Error::Ffmpeg(
            "ffmpeg is not installed; it's needed to join video and audio".into(),
        ))
    }
}

/// Copies the streams of `inputs` into `dest` without re-encoding. Returns
/// `false` when ffmpeg isn't installed.
async fn run_ffmpeg(inputs: &[&Path], extra: &[&str], dest: &Path, what: &str) -> Result<bool> {
    let mut command = tokio::process::Command::new("ffmpeg");
    command.args(["-y", "-loglevel", "error"]);
    for input in inputs {
        command.arg("-i").arg(input);
    }
    let status = command
        .args(["-c", "copy"])
        .args(extra)
        .arg(dest)
        .status()
        .await;
    match status {
        Ok(status) if status.success() => Ok(true),
        Ok(status) => Err(Error::Ffmpeg(format!("{what} failed ({status})"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::Io(e)),
    }
}

fn request_headers(format: &Format) -> HeaderMap {
    header_map(&format.headers)
}

fn header_map(pairs: &[(String, String)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    // Compressed responses would break byte ranges.
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    for (name, value) in pairs {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

/// Reads the total size from `Content-Range: bytes 0-0/12345`.
fn total_from_content_range(headers: &HeaderMap) -> Option<u64> {
    let value = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    value.rsplit_once('/')?.1.parse().ok().filter(|&n| n > 0)
}

fn part_path(dest: &Path, suffix: &str) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{suffix}"));
    dest.with_file_name(name)
}

async fn file_len(path: &Path) -> u64 {
    fs::metadata(path).await.map(|m| m.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_total_size() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 0-0/81974"));
        assert_eq!(total_from_content_range(&headers), Some(81974));
        headers.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 0-0/*"));
        assert_eq!(total_from_content_range(&headers), None);
    }

    #[test]
    fn part_paths_keep_extension() {
        let path = part_path(Path::new("/tmp/a_1.mp4"), "part0of4");
        assert_eq!(path, Path::new("/tmp/a_1.mp4.part0of4"));
    }
}

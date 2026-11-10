use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::future::try_join_all;
use reqwest::StatusCode;
use reqwest::header::{
    ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_RANGE, ETAG, HeaderMap, HeaderName, HeaderValue,
    IF_MATCH, RANGE,
};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};

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
    /// A new file starts; `already` is the number of bytes already available.
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
            download_single(client, media, format, &dest, label, options, progress).await?
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

// TikTok advertises several URLs for the same encoding. Retry those URLs only;
// never replace a requested quality with a smaller or silent variant.
async fn download_single(
    client: &Client,
    media: &Media,
    primary: &Format,
    dest: &Path,
    label: &str,
    options: &DownloadOptions,
    progress: &dyn Progress,
) -> Result<PathBuf> {
    let mut candidates = vec![primary];
    if media.platform == crate::Platform::TikTok {
        let mut seen = std::collections::HashSet::from([primary.url.as_str()]);
        candidates.extend(
            media
                .formats
                .iter()
                .rev()
                .filter(|f| {
                    f.width == primary.width
                        && f.height == primary.height
                        && f.codec == primary.codec
                        && f.bitrate == primary.bitrate
                        && f.has_audio == primary.has_audio
                        && f.has_video == primary.has_video
                        && f.protocol == primary.protocol
                        && f.ext == primary.ext
                        && seen.insert(f.url.as_str())
                })
                .take(3),
        );
    }
    if candidates.len() == 1 {
        return download_file(client, primary, dest, label, options, progress).await;
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut last_error = None;
    for candidate in candidates {
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let attempt = dest
            .parent()
            .unwrap_or(Path::new("."))
            .join(format!(".rjs-mirror-{}-{id}", std::process::id()));
        fs::create_dir(&attempt).await?;
        let target = attempt.join(dest.file_name().expect("media filename"));
        let result = download_file(client, candidate, &target, label, options, progress).await;
        match result {
            Ok(path) => {
                let final_path = dest.with_extension(path.extension().unwrap_or_default());
                let moved = fs::rename(&path, &final_path).await;
                let _ = fs::remove_dir_all(&attempt).await;
                moved?;
                return Ok(final_path);
            }
            Err(error) => {
                last_error = Some(error);
                fs::remove_dir_all(&attempt).await?;
            }
        }
    }
    Err(last_error.expect("at least one candidate"))
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
    let mut headers = request_headers(format);
    let mut fallback_parts = None;
    let small = format.size.is_some_and(|size| size < 2 * MIN_PIECE);
    if (options.connections <= 1 || small)
        && !fs::try_exists(part_path(dest, "parts-manifest")).await?
    {
        download_whole(client, &format.url, &headers, dest, label, progress).await?;
    } else {
        let probe = probe_range(client, &format.url, &headers).await?;
        reject_text_response(probe.headers())?;
        let total = if probe.status() == StatusCode::PARTIAL_CONTENT {
            content_range(probe.headers())
                .filter(|&(start, end, _)| start == 0 && end == 0)
                .map(|(_, _, total)| total)
        } else {
            None
        };
        // A strong validator pins every parallel request to the same representation.
        // Without one, use one response instead of risking mixed remote versions.
        let validator = probe
            .headers()
            .get(ETAG)
            .filter(|value| {
                value
                    .to_str()
                    .is_ok_and(|s| s.starts_with('"') && s.ends_with('"'))
            })
            .cloned();
        drop(probe);
        if let Some(validator) = validator.as_ref() {
            headers.insert(IF_MATCH, validator.clone());
        }

        match total.filter(|_| validator.is_some()) {
            Some(total) => {
                let result = download_pieces(
                    client,
                    &format.url,
                    &headers,
                    dest,
                    total,
                    label,
                    options,
                    progress,
                )
                .await;
                if result.is_err() {
                    // Some CDNs accept the probe but ignore later range requests.
                    // Restart with a complete response; never append it to a piece.
                    headers.remove(IF_MATCH);
                    download_whole(client, &format.url, &headers, dest, label, progress).await?;
                    fallback_parts = Some(total);
                }
            }
            None => download_whole(client, &format.url, &headers, dest, label, progress).await?,
        }
    }
    // A successful HTTP status does not guarantee a video/image body.
    let mut file = File::open(dest).await?;
    let mut prefix = [0u8; 1024];
    let n = file.read(&mut prefix).await?;
    if n == 0 || looks_like_error_page(&prefix[..n]) {
        drop(file);
        fs::remove_file(dest).await?;
        return Err(Error::InvalidMedia(
            "server returned a page instead of media",
        ));
    }
    if let Some(total) = fallback_parts {
        let count = (total / MIN_PIECE).clamp(1, options.connections.clamp(1, 32) as u64);
        for index in 0..count {
            let path = part_path(dest, &format!("part{index}of{count}"));
            match fs::remove_file(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        match fs::remove_file(part_path(dest, "parts-manifest")).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(dest.to_path_buf())
}

async fn probe_range(client: &Client, url: &str, headers: &HeaderMap) -> Result<reqwest::Response> {
    for attempt in 0..=2 {
        let response = client
            .http_for(url)
            .get(url)
            .headers(headers.clone())
            .header(RANGE, "bytes=0-0")
            .send()
            .await;
        let mut delay = Duration::from_millis(200 * (attempt + 1));
        let result = match response {
            Ok(response) => {
                if let Some(seconds) = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                {
                    delay = Duration::from_secs(seconds.min(2));
                }
                response.error_for_status().map_err(Error::from)
            }
            Err(error) => Err(Error::from(error)),
        };
        match result {
            Err(error) if attempt < 2 && error.is_retryable() => tokio::time::sleep(delay).await,
            result => return result,
        }
    }
    unreachable!("the last attempt returns")
}

/// Splits the file into ranges, downloads them in parallel into `.part` files,
/// then joins them. Parts are resumed only when their representation matches.
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
    let count = (total / MIN_PIECE).clamp(1, options.connections.clamp(1, 32) as u64);
    let piece = total.div_ceil(count);
    let pieces: Vec<(PathBuf, u64, u64)> = (0..count)
        .map(|i| {
            let start = i * piece;
            let end = start.saturating_add(piece).min(total) - 1;
            (part_path(dest, &format!("part{i}of{count}")), start, end)
        })
        .collect();

    // Bind saved parts to the exact representation and partitioning, without
    // storing the potentially sensitive URL itself.
    use sha2::{Digest, Sha256};
    let validator = headers
        .get(IF_MATCH)
        .ok_or(Error::InvalidMedia("range download has no validator"))?;
    let mut hash = Sha256::new();
    hash.update(url.as_bytes());
    hash.update([0]);
    hash.update(validator.as_bytes());
    hash.update(total.to_be_bytes());
    hash.update(count.to_be_bytes());
    let manifest_data = hash.finalize().to_vec();
    let manifest = part_path(dest, "parts-manifest");
    let matches = match fs::metadata(&manifest).await {
        Ok(meta) if meta.len() == 32 => fs::read(&manifest)
            .await
            .ok()
            .is_some_and(|data| data == manifest_data),
        _ => false,
    };
    if !matches {
        for (path, _, _) in &pieces {
            match fs::remove_file(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let temporary = part_path(dest, "parts-manifest.tmp");
        fs::write(&temporary, manifest_data).await?;
        fs::rename(temporary, &manifest).await?;
    }
    let mut already = 0;
    for (path, start, end) in &pieces {
        let len = file_len(path).await;
        if len > end - start + 1 {
            fs::remove_file(path).await?;
        } else {
            already += len;
        }
    }
    progress.start(label, Some(total), already);

    try_join_all(pieces.iter().map(|(path, start, end)| {
        fetch_range(client, url, headers, path, *start, *end, total, progress)
    }))
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
    let _ = fs::remove_file(manifest).await;
    Ok(())
}

/// Downloads bytes `start..=end` into `path`, resuming after network errors.
#[allow(clippy::too_many_arguments)]
async fn fetch_range(
    client: &Client,
    url: &str,
    headers: &HeaderMap,
    path: &Path,
    start: u64,
    end: u64,
    total: u64,
    progress: &dyn Progress,
) -> Result<()> {
    let want = end - start + 1;
    let mut failures = 0;
    loop {
        let have = file_len(path).await;
        if have > want {
            return Err(Error::InvalidMedia(
                "downloaded range exceeds its expected size",
            ));
        }
        if have == want {
            return Ok(());
        }

        let result = stream_range(
            client,
            url,
            headers,
            path,
            start + have,
            end,
            total,
            progress,
        )
        .await;
        if file_len(path).await == want {
            return result;
        }
        if matches!(&result, Err(Error::InvalidMedia(_)))
            || matches!(&result, Err(Error::Network(error)) if error.status().is_some())
        {
            return result;
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

#[allow(clippy::too_many_arguments)]
async fn stream_range(
    client: &Client,
    url: &str,
    headers: &HeaderMap,
    path: &Path,
    start: u64,
    end: u64,
    total: u64,
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
    reject_text_response(resp.headers())?;
    if resp.status() != StatusCode::PARTIAL_CONTENT {
        return Err(Error::InvalidMedia(
            "server ignored the requested byte range",
        ));
    }

    if content_range(resp.headers()) != Some((start, end, total)) {
        return Err(Error::InvalidMedia(
            "server returned an inconsistent byte range",
        ));
    }
    reject_encoded_response(resp.headers())?;
    if let (Some(expected), Some(actual)) = (headers.get(IF_MATCH), resp.headers().get(ETAG))
        && expected != actual
    {
        return Err(Error::InvalidMedia(
            "server returned a different representation",
        ));
    }
    let expected = end - start + 1;
    if resp
        .content_length()
        .is_some_and(|length| length != expected)
    {
        return Err(Error::InvalidMedia(
            "byte range has an inconsistent content length",
        ));
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    let mut out = BufWriter::with_capacity(256 * 1024, file);
    let result = async {
        let mut received = 0u64;
        while let Some(chunk) = resp.chunk().await? {
            received += chunk.len() as u64;
            if received > expected {
                return Err(Error::InvalidMedia(
                    "byte range body exceeds its expected size",
                ));
            }
            out.write_all(&chunk).await?;
            progress.advance(chunk.len() as u64);
        }
        if received != expected {
            return Err(Error::InvalidMedia("byte range body is truncated"));
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
    for attempt in 0..=2 {
        match download_whole_once(client, url, headers, dest, label, progress).await {
            Err(error) if attempt < 2 && error.is_retryable() => {
                tokio::time::sleep(Duration::from_millis(200 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    unreachable!("the last attempt returns")
}

async fn download_whole_once(
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
    reject_text_response(resp.headers())?;
    if resp.status() != StatusCode::OK {
        return Err(Error::InvalidMedia(
            "server did not return a complete response",
        ));
    }
    reject_encoded_response(resp.headers())?;
    let expected = resp.content_length();
    progress.start(label, expected, 0);

    let part = part_path(dest, "part");
    let mut out = BufWriter::with_capacity(256 * 1024, File::create(&part).await?);
    let mut received = 0u64;
    while let Some(chunk) = resp.chunk().await? {
        received += chunk.len() as u64;
        if expected.is_some_and(|length| received > length) {
            return Err(Error::InvalidMedia("response exceeds its expected size"));
        }
        out.write_all(&chunk).await?;
        progress.advance(chunk.len() as u64);
    }
    if expected.is_some_and(|length| received != length) {
        return Err(Error::InvalidMedia("response body is truncated"));
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
    command.kill_on_drop(true);
    command.args(["-y", "-loglevel", "error"]);
    for input in inputs {
        command.arg("-i").arg(input);
    }
    let extension = dest.extension().and_then(|s| s.to_str()).unwrap_or("mp4");
    let staged = dest.with_extension(format!("rjs-ffmpeg.{extension}"));
    let status = command
        .args(["-c", "copy"])
        .args(extra)
        .arg(&staged)
        .status()
        .await;
    match status {
        Ok(status) if status.success() => {
            fs::rename(staged, dest).await?;
            Ok(true)
        }
        Ok(status) => {
            let _ = fs::remove_file(staged).await;
            Err(Error::Ffmpeg(format!("{what} failed ({status})")))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::Io(e)),
    }
}

fn reject_text_response(headers: &HeaderMap) -> Result<()> {
    let content_type = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if matches!(
        content_type.as_str(),
        "text/html" | "application/xhtml+xml" | "application/json" | "text/xml" | "application/xml"
    ) {
        return Err(Error::InvalidMedia(
            "server returned a page instead of media",
        ));
    }
    Ok(())
}

fn looks_like_error_page(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    let prefix = text
        .trim_start_matches('\u{feff}')
        .trim_start()
        .to_ascii_lowercase();
    [
        "<!doctype",
        "<html",
        "<head",
        "<body",
        "<?xml",
        "<error",
        "{",
        "[",
    ]
    .iter()
    .any(|start| prefix.starts_with(start))
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
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    headers.remove(RANGE);
    headers.remove(IF_MATCH);
    headers
}

fn reject_encoded_response(headers: &HeaderMap) -> Result<()> {
    if headers
        .get(CONTENT_ENCODING)
        .is_some_and(|value| value != "identity")
    {
        return Err(Error::InvalidMedia(
            "server returned encoded bytes for an identity request",
        ));
    }
    Ok(())
}

fn content_range(headers: &HeaderMap) -> Option<(u64, u64, u64)> {
    let value = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, total) = (
        start.parse::<u64>().ok()?,
        end.parse::<u64>().ok()?,
        total.parse::<u64>().ok()?,
    );
    (start <= end && end < total).then_some((start, end, total))
}

/// Reads the total size from `Content-Range: bytes 0-0/12345`.
#[cfg(test)]
fn total_from_content_range(headers: &HeaderMap) -> Option<u64> {
    content_range(headers).map(|(_, _, total)| total)
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

    #[tokio::test]
    async fn validates_ranges_and_resumes_local_http_response() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/video", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for response in [
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 1-2/4\r\nContent-Length: 2\r\nConnection: close\r\n\r\nbc",
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2-3/4\r\nContent-Length: 2\r\nConnection: close\r\n\r\ncd",
            ] {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = [0; 2048];
                let read = socket.read(&mut request).unwrap();
                assert!(read > 0);
                socket.write_all(response.as_bytes()).unwrap();
            }
        });
        let path = std::env::temp_dir().join(format!("rjs-range-test-{}", std::process::id()));
        tokio::fs::write(&path, b"ab").await.unwrap();
        let client = Client::new().unwrap();
        assert!(
            stream_range(
                &client,
                &url,
                &HeaderMap::new(),
                &path,
                2,
                3,
                4,
                &NoProgress
            )
            .await
            .is_err()
        );
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"ab");
        stream_range(
            &client,
            &url,
            &HeaderMap::new(),
            &path,
            2,
            3,
            4,
            &NoProgress,
        )
        .await
        .unwrap();
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"abcd");
        tokio::fs::remove_file(path).await.unwrap();
        server.join().unwrap();
    }

    fn local_server(
        responses: Vec<&'static str>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/video", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().unwrap();
                let mut data = [0; 4096];
                let n = socket.read(&mut data).unwrap();
                requests.push(String::from_utf8_lossy(&data[..n]).to_ascii_lowercase());
                socket.write_all(response.as_bytes()).unwrap();
            }
            requests
        });
        (url, server)
    }

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("rjs-{}-{name}", std::process::id()))
    }

    #[tokio::test]
    async fn rejects_inconsistent_parallel_responses_and_truncated_whole() {
        let (url, server) = local_server(vec![
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-1/4\r\nContent-Length: 2\r\n\r\nab",
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2-2/4\r\nContent-Length: 1\r\n\r\nc",
            "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nabcd",
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2-3/4\r\nETag: \"new\"\r\nContent-Length: 2\r\n\r\ncd",
            "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nab",
        ]);
        let client = Client::new().unwrap();
        let path = test_path("bad-range");
        let mut headers = HeaderMap::new();
        headers.insert(IF_MATCH, HeaderValue::from_static("\"old\""));
        for _ in 0..4 {
            assert!(
                stream_range(&client, &url, &headers, &path, 2, 3, 4, &NoProgress)
                    .await
                    .is_err()
            );
            assert!(!path.exists());
        }
        assert!(
            download_whole(&client, &url, &HeaderMap::new(), &path, "test", &NoProgress)
                .await
                .is_err()
        );
        assert!(!path.exists());
        let _ = fs::remove_file(part_path(&path, "part")).await;
        server.join().unwrap();
    }

    #[tokio::test]
    async fn ignored_range_restarts_with_a_whole_response() {
        let (url, server) = local_server(vec![
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/4\r\nETag: \"v1\"\r\nContent-Length: 1\r\nConnection: close\r\n\r\na",
            "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nwxyz",
            "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nwxyz",
        ]);
        let path = test_path("fallback.mp4");
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        hash.update(url.as_bytes());
        hash.update([0]);
        hash.update(b"\"v1\"");
        hash.update(4u64.to_be_bytes());
        hash.update(1u64.to_be_bytes());
        fs::write(part_path(&path, "parts-manifest"), hash.finalize().to_vec())
            .await
            .unwrap();
        fs::write(part_path(&path, "part0of1"), b"a").await.unwrap();
        let client = Client::new().unwrap();
        download_file(
            &client,
            &Format::mp4("test", &url),
            &path,
            "test",
            &DownloadOptions::default(),
            &NoProgress,
        )
        .await
        .unwrap();
        assert_eq!(fs::read(&path).await.unwrap(), b"wxyz");
        let requests = server.join().unwrap();
        assert!(requests[1].contains("if-match: \"v1\""));
        assert!(!requests[2].contains("if-match:"));
        assert!(!part_path(&path, "part0of1").exists());
        assert!(!part_path(&path, "parts-manifest").exists());
        fs::remove_file(&path).await.unwrap();
    }

    #[tokio::test]
    async fn fresh_single_connection_uses_one_request() {
        let (url, server) = local_server(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nwxyz",
        ]);
        let path = test_path("single.mp4");
        let options = DownloadOptions {
            connections: 1,
            ..Default::default()
        };
        download_file(
            &Client::new().unwrap(),
            &Format::mp4("test", &url),
            &path,
            "test",
            &options,
            &NoProgress,
        )
        .await
        .unwrap();
        assert_eq!(fs::read(&path).await.unwrap(), b"wxyz");
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].contains("range:"));
        fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn known_small_file_avoids_probe_with_default_connections() {
        let (url, server) = local_server(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nwxyz",
        ]);
        let path = test_path("small-direct.mp4");
        let format = Format {
            size: Some(4),
            ..Format::mp4("test", url)
        };
        download_file(
            &Client::new().unwrap(),
            &format,
            &path,
            "test",
            &DownloadOptions::default(),
            &NoProgress,
        )
        .await
        .unwrap();
        assert_eq!(fs::read(&path).await.unwrap(), b"wxyz");
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].contains("range:"));
        fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn probe_retries_transient_status() {
        let (url, server) = local_server(vec![
            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/4\r\nETag: \"v1\"\r\nContent-Length: 1\r\nConnection: close\r\n\r\na",
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/4\r\nETag: \"v1\"\r\nContent-Length: 4\r\nConnection: close\r\n\r\nwxyz",
        ]);
        let path = test_path("probe-retry.mp4");
        download_file(
            &Client::new().unwrap(),
            &Format::mp4("test", &url),
            &path,
            "test",
            &DownloadOptions::default(),
            &NoProgress,
        )
        .await
        .unwrap();
        assert_eq!(fs::read(&path).await.unwrap(), b"wxyz");
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].contains("range: bytes=0-0"));
        assert!(requests[1].contains("range: bytes=0-0"));
        assert!(requests[2].contains("if-match: \"v1\""));
        fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn whole_download_retries_transient_status() {
        let (url, server) = local_server(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nwxyz",
        ]);
        let path = test_path("retry.mp4");
        let options = DownloadOptions {
            connections: 1,
            ..Default::default()
        };
        download_file(
            &Client::new().unwrap(),
            &Format::mp4("test", &url),
            &path,
            "test",
            &options,
            &NoProgress,
        )
        .await
        .unwrap();
        assert_eq!(fs::read(&path).await.unwrap(), b"wxyz");
        assert_eq!(server.join().unwrap().len(), 2);
        fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn matching_manifest_resumes_partial_file() {
        use sha2::{Digest, Sha256};
        let (url, server) = local_server(vec![
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2-3/4\r\nETag: \"v1\"\r\nContent-Length: 2\r\nConnection: close\r\n\r\ncd",
        ]);
        let path = test_path("resume.mp4");
        let mut hash = Sha256::new();
        hash.update(url.as_bytes());
        hash.update([0]);
        hash.update(b"\"v1\"");
        hash.update(4u64.to_be_bytes());
        hash.update(1u64.to_be_bytes());
        fs::write(part_path(&path, "parts-manifest"), hash.finalize().to_vec())
            .await
            .unwrap();
        fs::write(part_path(&path, "part0of1"), b"ab")
            .await
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(IF_MATCH, HeaderValue::from_static("\"v1\""));
        download_pieces(
            &Client::new().unwrap(),
            &url,
            &headers,
            &path,
            4,
            "test",
            &DownloadOptions::default(),
            &NoProgress,
        )
        .await
        .unwrap();
        assert_eq!(fs::read(&path).await.unwrap(), b"abcd");
        assert!(server.join().unwrap()[0].contains("range: bytes=2-3"));
        fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn changed_validator_discards_old_parts() {
        let (url, server) = local_server(vec![
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/4\r\nETag: \"v2\"\r\nContent-Length: 4\r\nConnection: close\r\n\r\nwxyz",
        ]);
        let path = test_path("changed.mp4");
        fs::write(part_path(&path, "parts-manifest"), [0u8; 32])
            .await
            .unwrap();
        fs::write(part_path(&path, "part0of1"), b"ab")
            .await
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(IF_MATCH, HeaderValue::from_static("\"v2\""));
        download_pieces(
            &Client::new().unwrap(),
            &url,
            &headers,
            &path,
            4,
            "test",
            &DownloadOptions::default(),
            &NoProgress,
        )
        .await
        .unwrap();
        assert_eq!(fs::read(&path).await.unwrap(), b"wxyz");
        assert!(server.join().unwrap()[0].contains("range: bytes=0-3"));
        fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn failed_ffmpeg_leaves_no_completed_file() {
        let input = test_path("invalid-input");
        let output = test_path("ffmpeg.mp4");
        fs::write(&input, b"not media").await.unwrap();
        let result = run_ffmpeg(&[&input], &[], &output, "test").await;
        assert!(result.is_err() || matches!(result, Ok(false)));
        assert!(!output.exists());
        assert!(!output.with_extension("rjs-ffmpeg.mp4").exists());
        fs::remove_file(input).await.unwrap();
    }

    #[test]
    fn rejects_error_responses_even_with_success_status() {
        for mime in [
            "text/html; charset=utf-8",
            "application/json",
            "application/xml",
            "text/xml",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(
                reqwest::header::CONTENT_TYPE,
                HeaderValue::from_str(mime).unwrap(),
            );
            assert!(reject_text_response(&headers).is_err());
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("video/mp4"),
        );
        assert!(reject_text_response(&headers).is_ok());
        assert!(reject_text_response(&HeaderMap::new()).is_ok());
    }

    #[test]
    fn detects_html_or_json_mislabeled_as_binary() {
        for body in [
            "  <!DOCTYPE html><html>blocked</html>",
            "\u{feff}<html>error</html>",
            "<?xml version=1.0?><Error>denied</Error>",
            "{\"error\":\"denied\"}",
        ] {
            assert!(looks_like_error_page(body.as_bytes()));
        }
        assert!(!looks_like_error_page(b"\x00\x00\x00\x18ftypisom"));
        assert!(!looks_like_error_page(b"\xff\xd8\xff\xe0JPEG"));
    }

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

//! Everything the tool prints.

use std::path::Path;
use std::time::Duration;

use console::{Style, StyledObject, Term, style, truncate_str};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use rjs_core::{Error, Format, Media, MediaKind, Platform, Profile, Progress, Selection};

/// Width of the item label column, e.g. `video 12`.
const LABEL: usize = 10;

fn width() -> usize {
    Term::stdout()
        .size_checked()
        .map_or(100, |(_, w)| usize::from(w))
        .clamp(40, 120)
}

fn badge(platform: Platform) -> StyledObject<&'static str> {
    let s = style(platform.name()).bold();
    match platform {
        Platform::X => s.white(),
        Platform::TikTok => s.magenta(),
        Platform::Instagram => s.yellow(),
        Platform::Facebook => s.blue(),
    }
}

fn dot() -> StyledObject<&'static str> {
    style("·").dim()
}

pub fn post_header(first: &Media, count: usize) {
    let mut line = format!(" {}", badge(first.platform));
    if let Some(author) = &first.author {
        // Facebook gives page names rather than usernames.
        let author = match first.platform {
            Platform::Facebook => author.clone(),
            _ => format!("@{author}"),
        };
        line += &format!(" {} {}", dot(), style(author).bold());
    }
    if count > 1 {
        line += &format!(" {} {}", dot(), style(format!("{count} items")).dim());
    }
    println!();
    println!("{line}");
    caption(first.title.as_deref());
    println!();
}

pub fn profile_header(profile: &Profile) {
    let mut line = format!(
        " {} {} {}",
        badge(profile.platform),
        dot(),
        style(format!("@{}", profile.username)).bold()
    );
    if let Some(name) = profile.name.as_deref().filter(|n| *n != profile.username) {
        line += &format!("  {name}");
    }
    println!();
    println!("{line}");
    caption(profile.bio.as_deref());

    let mut stats = Vec::new();
    if let Some(n) = profile.post_count {
        stats.push(format!("{} posts", compact(n)));
    }
    if let Some(n) = profile.follower_count {
        stats.push(format!("{} followers", compact(n)));
    }
    if profile.private {
        stats.push(style("private account").yellow().to_string());
    }
    if !stats.is_empty() {
        println!(" {}", style(stats.join(" · ")).dim());
    }
    println!();
}

fn caption(text: Option<&str>) {
    if let Some(text) = text {
        println!(" {}", truncate_str(text, width() - 2, "…"));
    }
}

/// `label` is what the row is about, e.g. `video 3/4` or `avatar`.
pub fn saved(label: &str, format: Option<&Format>, bytes: u64, path: &Path) {
    let dims = format.and_then(dimensions).unwrap_or_default();
    println!(
        "  {} {:<LABEL$} {:<11} {:>9}   {}",
        style("✓").green().bold(),
        label,
        style(dims).dim(),
        size(bytes),
        pretty_path(path),
    );
}

pub fn skipped(label: &str, path: &Path) {
    println!(
        "  {} {:<LABEL$} {:<21}   {}",
        style("•").dim(),
        style(label).dim(),
        style("already saved").dim(),
        pretty_path(path),
    );
}

pub fn item_failed(label: &str, error: &Error) {
    println!(
        "  {} {:<LABEL$} {}",
        style("✗").red().bold(),
        label,
        style(message(error)).red()
    );
}

pub fn failed(link: &str, error: &Error) {
    let heading = match error {
        Error::Extract { platform, .. } => badge(*platform).to_string(),
        Error::Cookies(_) => style("Couldn't load logins").red().bold().to_string(),
        _ => style("Couldn't download").red().bold().to_string(),
    };
    println!();
    println!(" {} {}", style("✗").red().bold(), heading);
    println!("   {}", message(error));
    if !link.is_empty() {
        println!("   {}", style(truncate_str(link, width() - 4, "…")).dim());
    }
}

pub fn cookies_loaded(count: usize, source: &str) {
    let text = match count {
        0 => format!("No logins for supported sites found in {source}"),
        n => format!("Using {n} login cookies from {source}"),
    };
    println!();
    println!(" {} {}", style("🔑").dim(), style(text).dim());
}

pub fn note(text: &str) {
    println!("  {}", style(text).dim());
}

fn message(error: &Error) -> String {
    match error {
        Error::Extract { message, .. } | Error::Cookies(message) => capitalize(message),
        Error::Unsupported(_) => "Not a supported site (TikTok, Instagram, Facebook or X)".into(),
        other => capitalize(&other.to_string()),
    }
}

pub struct Totals {
    pub saved: usize,
    pub skipped: usize,
    pub failed: usize,
    pub bytes: u64,
}

pub fn summary(totals: &Totals, elapsed: Duration, dir: &Path) {
    let mut parts = vec![format!("{} saved", totals.saved)];
    if totals.skipped > 0 {
        parts.push(format!("{} already saved", totals.skipped));
    }
    if totals.failed > 0 {
        parts.push(style(format!("{} failed", totals.failed)).red().to_string());
    }
    let title = if totals.failed > 0 {
        style("Finished with errors").yellow().bold()
    } else {
        style("Done").green().bold()
    };
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_owned());
    println!();
    println!(
        " {}  {} {} {}",
        title,
        parts.join(&format!(" {} ", dot())),
        dot(),
        style(format!(
            "{} in {} → {}",
            size(totals.bytes),
            clock(elapsed.as_secs_f64()),
            pretty_path(&dir)
        ))
        .dim()
    );
}

pub fn media_info(media: &Media, label: Option<&str>) {
    let mut line = format!("  {}", style(label.unwrap_or(media.kind.name())).bold());
    if let Some(d) = media.duration {
        line += &format!(" {} {}", dot(), clock(d));
    }

    // Pictures come in one size, so a table would only repeat it.
    if media.kind == MediaKind::Image {
        if let Some(Selection::Single(f)) = media.best() {
            if let Some(dims) = dimensions(f) {
                line += &format!(" {} {dims}", dot());
            }
            line += &format!(" {} {}", dot(), f.ext);
        }
        println!("{line}");
        return;
    }
    println!("{line}");

    let default: Vec<&str> = match media.best() {
        Some(Selection::Single(f)) => vec![&f.id],
        Some(Selection::Merge { video, audio }) => vec![&video.id, &audio.id],
        None => vec![],
    };
    let rows: Vec<[String; 6]> = media
        .formats
        .iter()
        .map(|f| {
            [
                f.id.clone(),
                f.ext.clone(),
                dimensions(f).unwrap_or_else(|| "–".into()),
                f.bitrate.map(bitrate).unwrap_or_else(|| "–".into()),
                f.codec.clone().unwrap_or_else(|| "–".into()),
                match (f.has_video, f.has_audio) {
                    (true, true) => "video + audio",
                    (true, false) => "video only",
                    (false, true) => "audio only",
                    (false, false) => "image",
                }
                .into(),
            ]
        })
        .collect();

    let head = [
        "FORMAT",
        "TYPE",
        "RESOLUTION",
        "BITRATE",
        "CODEC",
        "CONTENT",
    ];
    // Leave out columns the site gave no information for.
    let shown: Vec<usize> = (0..head.len())
        .filter(|&col| rows.iter().any(|row| row[col] != "–"))
        .collect();
    let mut widths = head.map(str::len);
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(console::measure_text_width(cell));
        }
    }
    let render = |cells: &[String; 6], style: &Style| {
        shown
            .iter()
            .map(|&col| {
                let w = widths[col];
                style.apply_to(format!("{:<w$}", cells[col])).to_string()
            })
            .collect::<Vec<_>>()
            .join("  ")
    };

    println!(
        "    {}",
        render(&head.map(String::from), &Style::new().dim())
    );
    for (f, row) in media.formats.iter().zip(&rows) {
        if default.contains(&f.id.as_str()) {
            println!(
                "  {} {}  {}",
                style("▸").cyan().bold(),
                render(row, &Style::new().bold()),
                style("default").cyan()
            );
        } else {
            println!("    {}", render(row, &Style::new()));
        }
    }
    println!();
}

pub fn hint(text: &str) {
    println!(" {}", style(text).dim());
}

/// Progress bar for one file; stays invisible until the download actually starts.
pub struct Bar(ProgressBar);

impl Bar {
    pub fn new(label: &str) -> Self {
        let bar = ProgressBar::hidden();
        bar.set_prefix(label.to_owned());
        Self(bar)
    }

    pub fn clear(&self) {
        self.0.finish_and_clear();
    }
}

impl Progress for Bar {
    fn start(&self, part: &str, total: Option<u64>, already: u64) {
        let bar = &self.0;
        bar.set_draw_target(ProgressDrawTarget::stderr());
        bar.reset();
        let template = match total {
            Some(total) => {
                bar.set_length(total);
                "  {spinner:.cyan} {prefix:<10} {bar:28.cyan/dim} {decimal_bytes:>9} / {decimal_total_bytes:<9} {decimal_bytes_per_sec:>11}  {msg}"
            }
            None => {
                bar.unset_length();
                "  {spinner:.cyan} {prefix:<10} {decimal_bytes:>9} {decimal_bytes_per_sec:>11}  {msg}"
            }
        };
        bar.set_style(
            ProgressStyle::with_template(template)
                .expect("valid template")
                .progress_chars("━╸─")
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
        );
        bar.enable_steady_tick(Duration::from_millis(100));
        // Videos that come as separate streams download their audio second.
        bar.set_message(if part == "audio" {
            style("audio").dim().to_string()
        } else {
            String::new()
        });
        bar.set_position(already);
    }

    fn advance(&self, bytes: u64) {
        self.0.inc(bytes);
    }
}

/// The row label for item `index` (1-based) of `count` in a post.
pub fn item_label(media: &Media, index: usize, count: usize) -> String {
    let kind = match media.kind {
        MediaKind::Video => "video",
        MediaKind::Image => "photo",
    };
    if count > 1 {
        format!("{kind} {index}/{count}")
    } else {
        kind.into()
    }
}

fn dimensions(f: &Format) -> Option<String> {
    Some(format!("{}×{}", f.width?, f.height?))
}

pub fn size(bytes: u64) -> String {
    let b = bytes as f64;
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..1_000_000 => format!("{:.0} kB", b / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1} MB", b / 1e6),
        _ => format!("{:.2} GB", b / 1e9),
    }
}

fn bitrate(bps: u64) -> String {
    if bps >= 1_000_000 {
        format!("{:.1} Mb/s", bps as f64 / 1e6)
    } else {
        format!("{} kb/s", bps / 1000)
    }
}

fn compact(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}K", n as f64 / 1e3).replace(".0K", "K"),
        _ => format!("{:.1}M", n as f64 / 1e6).replace(".0M", "M"),
    }
}

fn clock(seconds: f64) -> String {
    if seconds < 60.0 {
        return format!("{seconds:.1}s").replace(".0s", "s");
    }
    let s = seconds.round() as u64;
    match s / 3600 {
        0 => format!("{}:{:02}", s / 60, s % 60),
        h => format!("{h}:{:02}:{:02}", s / 60 % 60, s % 60),
    }
}

/// `./a/b.mp4` → `a/b.mp4`, `/home/me/x` → `~/x`, with the file name highlighted.
fn pretty_path(path: &Path) -> String {
    let mut shown = path.strip_prefix(".").unwrap_or(path).display().to_string();
    if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from)
        && let Ok(rest) = path.strip_prefix(&home)
    {
        shown = format!("~/{}", rest.display());
    }
    if shown.is_empty() {
        return ".".into();
    }
    match shown.rsplit_once('/') {
        Some((dir, file)) => format!("{}{}", style(format!("{dir}/")).dim(), file),
        None => shown,
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_numbers() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(2_345_678), "2.3 MB");
        assert_eq!(compact(1_000), "1K");
        assert_eq!(compact(1_234_567), "1.2M");
        assert_eq!(clock(4.46), "4.5s");
        assert_eq!(clock(324.5), "5:25");
        assert_eq!(clock(3725.0), "1:02:05");
        assert_eq!(bitrate(832_000), "832 kb/s");
        assert_eq!(bitrate(2_176_000), "2.2 Mb/s");
    }
}

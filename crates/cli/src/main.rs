mod ui;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Parser;
use rjs_core::{Client, DownloadOptions, Media, Result, Selection};

/// Download videos and pictures from TikTok, Instagram, Facebook and X.
///
/// Give a post link to download it, or a TikTok profile link to download the
/// profile picture and every post into a folder named after the account.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Post or profile links.
    #[arg(required = true, value_name = "LINK")]
    links: Vec<String>,

    /// Folder to save into.
    #[arg(short, long, default_value = ".", value_name = "DIR")]
    output: PathBuf,

    /// Show details and available formats without downloading.
    #[arg(short, long)]
    info: bool,

    /// Format to download: an ID shown by --info (e.g. play, hd) or a quality (e.g. 720).
    #[arg(short, long)]
    format: Option<String>,

    /// Download only the profile picture (TikTok profile or post links).
    #[arg(short, long)]
    avatar: bool,

    /// For profile links: download at most this many posts, newest first.
    #[arg(short, long, value_name = "N")]
    limit: Option<usize>,

    /// Use the logins from a browser: chrome, chromium, brave, edge or firefox.
    /// Add a profile after a colon, e.g. "chrome:Profile 1".
    #[arg(short = 'b', long, value_name = "BROWSER", conflicts_with = "cookies")]
    cookies_from_browser: Option<String>,

    /// Use the logins from a cookies.txt file (Netscape format).
    #[arg(long, value_name = "FILE")]
    cookies: Option<PathBuf>,

    /// Download again even if the file already exists.
    #[arg(long)]
    force: bool,

    /// Parallel connections per file.
    #[arg(short, long, default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..=32))]
    connections: u16,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let client = match Client::new() {
        Ok(client) => client,
        Err(e) => {
            ui::failed("", &e);
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = load_cookies(&client, &args) {
        ui::failed("", &e);
        return ExitCode::FAILURE;
    }

    let mut session = Session {
        options: DownloadOptions {
            connections: args.connections.into(),
            format: args.format.clone(),
            skip_existing: !args.force,
        },
        totals: ui::Totals {
            saved: 0,
            skipped: 0,
            failed: 0,
            bytes: 0,
        },
        client,
        args,
    };
    let started = Instant::now();

    let mut link_failed = false;
    for link in &session.args.links.clone() {
        if let Err(e) = session.run(link).await {
            ui::failed(link, &e);
            link_failed = true;
        }
    }

    let t = &session.totals;
    if !session.args.info && t.saved + t.skipped + t.failed > 1 {
        ui::summary(t, started.elapsed(), &session.args.output);
    }
    println!();
    if link_failed || t.failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

struct Session {
    client: Client,
    args: Args,
    options: DownloadOptions,
    totals: ui::Totals,
}

impl Session {
    async fn run(&mut self, link: &str) -> Result<()> {
        if self.args.avatar {
            self.avatar(link).await
        } else if rjs_core::is_profile(link) {
            self.profile(link).await
        } else {
            self.post(link).await
        }
    }

    async fn post(&mut self, link: &str) -> Result<()> {
        let items = rjs_core::extract(&self.client, link).await?;
        ui::post_header(&items[0], items.len());

        let output = self.args.output.clone();
        for (i, media) in items.iter().enumerate() {
            let label = ui::item_label(media, i + 1, items.len());
            if self.args.info {
                ui::media_info(media, Some(&label));
            } else {
                self.save(media, &label, &output).await;
            }
        }
        Ok(())
    }

    async fn avatar(&mut self, link: &str) -> Result<()> {
        let avatar = rjs_core::avatar(&self.client, link).await?;
        ui::post_header(&avatar, 1);
        if self.args.info {
            ui::media_info(&avatar, Some("avatar"));
        } else {
            let output = self.args.output.clone();
            self.save(&avatar, "avatar", &output).await;
        }
        Ok(())
    }

    async fn profile(&mut self, link: &str) -> Result<()> {
        let mut profile = rjs_core::profile(&self.client, link).await?;
        ui::profile_header(&profile);
        if self.args.info {
            ui::hint(
                "Run without --info to download the profile picture and every post, or add --limit N.",
            );
            return Ok(());
        }

        let dir = self.args.output.join(&profile.username);
        create_dir(&dir)?;
        if let Some(avatar) = profile.avatar.take() {
            self.save(&avatar, "avatar", &dir).await;
        }

        let limit = self.args.limit.unwrap_or(usize::MAX);
        let mut posts = 0;
        let mut last_post = None;
        'pages: while let Some(page) = profile.next_page(&self.client).await? {
            for media in &page {
                // Slideshow pictures share their post's ID prefix and count as one post.
                let post = media.id.split('_').next().map(str::to_owned);
                if post != last_post {
                    if posts == limit {
                        break 'pages;
                    }
                    posts += 1;
                    last_post = post;
                }
                let label = format!("{} {posts}", ui::item_label(media, 1, 1));
                self.save(media, &label, &dir).await;
            }
            // Be gentle with TikTok between pages.
            tokio::time::sleep(Duration::from_millis(600)).await;
        }

        if posts == 0 {
            ui::note(if profile.private {
                "This account is private, so its posts can't be listed."
            } else {
                "No posts found."
            });
        }
        Ok(())
    }

    /// Downloads one item and prints how it went; failures don't stop the remaining items.
    async fn save(&mut self, media: &Media, label: &str, dir: &Path) {
        let format = match media.select(self.options.format.as_deref()) {
            Ok(Selection::Single(f)) | Ok(Selection::Merge { video: f, .. }) => Some(f.clone()),
            Err(_) => None,
        };
        if let Err(e) = create_dir(dir) {
            ui::item_failed(label, &e);
            self.totals.failed += 1;
            return;
        }

        let bar = ui::Bar::new(label);
        let result = rjs_core::download_media(&self.client, media, dir, &self.options, &bar).await;
        bar.clear();
        match result {
            Ok(saved) if saved.skipped => {
                ui::skipped(label, &saved.path);
                self.totals.skipped += 1;
            }
            Ok(saved) => {
                let bytes = std::fs::metadata(&saved.path).map_or(0, |m| m.len());
                ui::saved(label, format.as_ref(), bytes, &saved.path);
                self.totals.saved += 1;
                self.totals.bytes += bytes;
            }
            Err(e) => {
                ui::item_failed(label, &e);
                self.totals.failed += 1;
            }
        }
    }
}

fn load_cookies(client: &Client, args: &Args) -> Result<()> {
    let (cookies, source) = match (&args.cookies_from_browser, &args.cookies) {
        (Some(browser), _) => (rjs_core::cookies::from_browser(browser)?, browser.clone()),
        (None, Some(file)) => (
            rjs_core::cookies::from_file(file)?,
            file.display().to_string(),
        ),
        (None, None) => return Ok(()),
    };
    let added = client.add_cookies(&cookies);
    ui::cookies_loaded(added, &source);
    Ok(())
}

fn create_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(rjs_core::Error::Io)
}

use crate::extractors::Pager;
use crate::{Client, Media, Platform, Result};

/// An account, whose posts can be fetched page by page with [`Profile::next_page`].
pub struct Profile {
    pub platform: Platform,
    pub username: String,
    pub name: Option<String>,
    pub bio: Option<String>,
    pub post_count: Option<u64>,
    pub follower_count: Option<u64>,
    pub private: bool,
    pub avatar: Option<Media>,
    pub(crate) pager: Pager,
}

impl Profile {
    /// The next batch of media, newest first. A batch may be empty; `None` means there are no more.
    pub async fn next_page(&mut self, client: &Client) -> Result<Option<Vec<Media>>> {
        self.pager.next(client).await
    }
}

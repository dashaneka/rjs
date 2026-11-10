//! Cooperative cancellation for embedding applications.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;

#[derive(Debug, Default)]
struct Inner {
    cancelled: AtomicBool,
    notify: Notify,
}

/// Cloneable, one-way cancellation shared by one or many operations.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<Inner>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancellation is idempotent. All current and future waiters are notified.
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wakes_all_waiters_and_remembers_early_cancellation() {
        let token = CancellationToken::new();
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let token = token.clone();
                tokio::spawn(async move { token.cancelled().await })
            })
            .collect();
        tokio::task::yield_now().await;
        token.cancel();
        token.cancel();
        for task in tasks {
            tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap();
        }
        token.cancelled().await;
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn cancelled_extraction_does_not_start_the_operation() {
        let token = CancellationToken::new();
        token.cancel();
        let client = crate::Client::new().unwrap();
        let result = crate::extract_with_cancel(&client, "not a supported link", &token).await;
        assert!(matches!(result, Err(crate::Error::Cancelled)));
    }
}

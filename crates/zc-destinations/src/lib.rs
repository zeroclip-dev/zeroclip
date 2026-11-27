//! # zc-destinations — where clips go
//!
//! Every upload target implements one trait, [`Destination`], and the editor's
//! drop-target row is rendered from the [`Registry`] — adding a platform is one
//! `impl`, not a UI change.
//!
//! Build order: **Discord first** — a webhook POST, no OAuth, no
//! quota — to prove the drag → upload → copy-link plumbing in an afternoon; then
//! **YouTube with bring-your-own credentials**, so the feature never waits on
//! Google's compliance audit.
//!
//! Uploads are async (they'll run on `tokio`, added in Phase 6). The trait returns
//! boxed futures so it stays object-safe.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use zc_core::Hns;
use zc_core::ports::Emit;

pub mod discord;
pub mod youtube;

/// A boxed, sendable future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Whether a destination is ready to receive uploads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthState {
    /// No credentials needed (Discord webhooks, local folders).
    NotRequired,
    /// Signed in and ready.
    Ready,
    /// The user has to configure it first (e.g. paste YouTube API credentials).
    NeedsSetup,
    /// Credentials expired; sign in again.
    Expired,
}

/// A clip about to be uploaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipUpload {
    /// The (remuxed, shareable) file.
    pub path: PathBuf,
    /// Its size.
    pub size_bytes: u64,
    /// Its length.
    pub duration: Hns,
    /// Title / message to post with it.
    pub title: String,
}

/// Upload progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    /// Bytes sent so far.
    pub sent_bytes: u64,
    /// Total bytes.
    pub total_bytes: u64,
}

/// Receives progress updates; must be cheap.
pub type ProgressSink = Emit<Progress>;

/// Where an uploaded clip can be viewed. Copied to the clipboard on success —
/// that's what the user actually wants every time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadedUrl(pub String);

/// Why an upload failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UploadError {
    /// Bigger than the destination accepts.
    #[error("{size} bytes is over this destination's {limit}-byte limit")]
    TooLarge {
        /// Clip size.
        size: u64,
        /// The limit.
        limit: u64,
    },
    /// Missing or expired credentials.
    #[error("not signed in: {0}")]
    NotAuthorized(String),
    /// The service refused the clip (content, format, quota).
    #[error("rejected: {0}")]
    Rejected(String),
    /// Connection problems; retrying may help.
    #[error("network error: {0}")]
    Network(String),
    /// The user cancelled.
    #[error("cancelled")]
    Cancelled,
}

/// An upload target. See the [crate docs](crate).
pub trait Destination: Send + Sync {
    /// Stable identifier, stored with upload records (`"discord"`, `"youtube"`).
    fn id(&self) -> &'static str;

    /// Name shown on the drop target.
    fn name(&self) -> &str;

    /// Largest accepted file, if limited. Drop targets show this *before* the drop
    /// and grey out when a clip won't fit.
    fn max_size(&self) -> Option<u64>;

    /// Whether uploads can start right now.
    fn auth_state(&self) -> AuthState;

    /// Uploads `clip`, reporting progress, and returns its URL.
    fn upload<'a>(
        &'a self,
        clip: &'a ClipUpload,
        progress: ProgressSink,
    ) -> BoxFuture<'a, Result<UploadedUrl, UploadError>>;
}

/// Whether a clip of `size_bytes` can go to `destination` (drives greying-out).
#[must_use]
pub fn fits(destination: &dyn Destination, size_bytes: u64) -> bool {
    let _ = (destination, size_bytes);
    todo!("Phase 6: compare against max_size (see the `fits_*` spec)")
}

/// The registered destinations, in display order.
#[derive(Default)]
pub struct Registry {
    destinations: Vec<Box<dyn Destination>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.destinations.iter().map(|d| d.id())).finish()
    }
}

impl Registry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a destination at the end of the row.
    pub fn register(&mut self, destination: Box<dyn Destination>) {
        self.destinations.push(destination);
    }

    /// All destinations, in display order.
    pub fn iter(&self) -> impl Iterator<Item = &dyn Destination> + '_ {
        self.destinations.iter().map(AsRef::as_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Option<u64>);

    impl Destination for Fixed {
        fn id(&self) -> &'static str {
            "fixed"
        }
        fn name(&self) -> &str {
            "Fixed"
        }
        fn max_size(&self) -> Option<u64> {
            self.0
        }
        fn auth_state(&self) -> AuthState {
            AuthState::NotRequired
        }
        fn upload<'a>(
            &'a self,
            _clip: &'a ClipUpload,
            _progress: ProgressSink,
        ) -> BoxFuture<'a, Result<UploadedUrl, UploadError>> {
            Box::pin(async { Ok(UploadedUrl("https://example.com/clip".into())) })
        }
    }

    #[test]
    fn registry_keeps_display_order() {
        let mut r = Registry::new();
        r.register(Box::new(Fixed(None)));
        r.register(Box::new(discord::DiscordWebhook::new("https://discord.com/api/webhooks/1/x")));
        let ids: Vec<&str> = r.iter().map(Destination::id).collect();
        assert_eq!(ids, ["fixed", "discord"]);
    }

    #[test]
    #[ignore = "spec: fits (Phase 6)"]
    fn fits_respects_the_size_limit() {
        assert!(fits(&Fixed(None), u64::MAX), "unlimited");
        assert!(fits(&Fixed(Some(20)), 20));
        assert!(!fits(&Fixed(Some(20)), 21));
    }
}

//! Discord webhooks: the first destination (DESIGN §10, "Destination 1").
//!
//! A webhook upload is a multipart POST to a URL — no OAuth, no quota, no API
//! project, no audit. The catch is size: the free per-file cap has changed four
//! times in as many years (20 MB as of August 2026, Nitro Basic 50 MB, Nitro 500
//! MB). So the limit is a *learned* value, not a constant: start from
//! [`ASSUMED_LIMIT_BYTES`], and when Discord rejects a file as too large
//! (HTTP 413), remember a lower limit for that webhook.
//!
//! Clips over the limit need either a "compress for Discord" re-encode (the one
//! deliberate exception to the zero-encode rule) or a link fallback.

use std::sync::Mutex;

use crate::{
    AuthState, BoxFuture, ClipUpload, Destination, ProgressSink, UploadError, UploadedUrl,
};

/// Starting guess for the per-file limit on a free account. Refined at runtime.
pub const ASSUMED_LIMIT_BYTES: u64 = 20 * 1024 * 1024;

/// A Discord channel webhook.
#[derive(Debug)]
pub struct DiscordWebhook {
    url: String,
    learned_limit: Mutex<Option<u64>>,
}

impl DiscordWebhook {
    /// A destination posting to `url` (`https://discord.com/api/webhooks/<id>/<token>`).
    /// The URL is a secret: anyone holding it can post to the channel.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), learned_limit: Mutex::new(None) }
    }

    /// The webhook URL.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl Destination for DiscordWebhook {
    fn id(&self) -> &'static str {
        "discord"
    }

    fn name(&self) -> &str {
        "Discord"
    }

    fn max_size(&self) -> Option<u64> {
        let learned = *self.learned_limit.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Some(learned.unwrap_or(ASSUMED_LIMIT_BYTES))
    }

    fn auth_state(&self) -> AuthState {
        AuthState::NotRequired
    }

    fn upload<'a>(
        &'a self,
        clip: &'a ClipUpload,
        progress: ProgressSink,
    ) -> BoxFuture<'a, Result<UploadedUrl, UploadError>> {
        let _ = (clip, progress);
        todo!(
            "Phase 6: reqwest multipart POST (payload_json + files[0]) with ?wait=true; \
             on 413 lower `learned_limit`; return the attachment URL from the response"
        )
    }
}

//! YouTube with bring-your-own credentials.
//!
//! Two API constraints shape this:
//!
//! - **Unlisted is exactly what gets blocked.** Uploads from unverified API
//!   projects are forced to *private* until the project passes Google's
//!   compliance audit — useless for sharing.
//! - **Quota is per API project, not per user.** A `client_secret` shipped in the
//!   app would share one project's daily upload allowance across every user.
//!
//! So each user creates their own Google Cloud project and pastes its OAuth client
//! here; the audit is attempted in parallel and, if it passes, becomes the easy
//! path with BYO as the fallback. Uploads use the resumable protocol (survives a
//! pulled network cable) and are verified with `videos.list` before anything is
//! ever deleted locally.

use crate::{
    AuthState, BoxFuture, ClipUpload, Destination, ProgressSink, UploadError, UploadedUrl,
};

/// An OAuth client from the user's own Google Cloud project.
#[derive(Clone, PartialEq, Eq)]
pub struct ByoCredentials {
    /// OAuth client id.
    pub client_id: String,
    /// OAuth client secret. Never logged.
    pub client_secret: String,
}

impl std::fmt::Debug for ByoCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ByoCredentials")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

/// YouTube uploads (unlisted), using the user's own API project.
#[derive(Debug, Default)]
pub struct YouTube {
    credentials: Option<ByoCredentials>,
}

impl YouTube {
    /// Not configured yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// With the user's OAuth client.
    #[must_use]
    pub fn with_credentials(credentials: ByoCredentials) -> Self {
        Self { credentials: Some(credentials) }
    }
}

impl Destination for YouTube {
    fn id(&self) -> &'static str {
        "youtube"
    }

    fn name(&self) -> &str {
        "YouTube"
    }

    fn max_size(&self) -> Option<u64> {
        // 256 GB — effectively unlimited for clips. (Unverified accounts are
        // limited to 15 minutes, which is a duration limit, not a size limit.)
        None
    }

    fn auth_state(&self) -> AuthState {
        match self.credentials {
            None => AuthState::NeedsSetup,
            Some(_) => todo!("Phase 6: Ready / Expired from the stored refresh token"),
        }
    }

    fn upload<'a>(
        &'a self,
        clip: &'a ClipUpload,
        progress: ProgressSink,
    ) -> BoxFuture<'a, Result<UploadedUrl, UploadError>> {
        let _ = (clip, progress);
        todo!(
            "Phase 6: OAuth (loopback redirect), resumable videos.insert (unlisted), verify via videos.list"
        )
    }
}

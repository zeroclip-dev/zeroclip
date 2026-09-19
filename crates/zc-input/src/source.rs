use zc_core::ports::{Emit, InputSource, OsEvent, SourceError};
use zc_win::message_thread::MessageThread;

/// Raw Input + foreground tracking on a dedicated message thread.
#[derive(Debug, Default)]
pub struct RawInputSource {
    thread: Option<MessageThread>,
}

impl RawInputSource {
    /// A stopped input source.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl InputSource for RawInputSource {
    fn start(&mut self, events: Emit<OsEvent>) -> Result<(), SourceError> {
        let _ = events;
        todo!(
            "Phase 2: MessageThread::spawn with setup = raw_input::register + \
             foreground::install, handler = raw_input::decode on WM_INPUT → events"
        )
    }

    fn stop(&mut self) {
        // Dropping the thread posts WM_QUIT and joins it.
        self.thread = None;
    }
}

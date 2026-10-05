use std::sync::Arc;

use windows::Win32::UI::WindowsAndMessaging::WM_INPUT;
use zc_core::ports::{Emit, InputSource, OsEvent, SourceError};
use zc_win::message_thread::MessageThread;

use crate::{foreground, raw_input};

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
        self.stop();
        let fg = Arc::clone(&events);
        let thread = MessageThread::spawn(
            "zc-input",
            move |hwnd| {
                raw_input::register(hwnd)?;
                foreground::install(move |e| fg(e))
            },
            move |m| {
                if m.msg == WM_INPUT {
                    raw_input::decode(m.lparam, |e| events(e));
                }
            },
        )?;
        self.thread = Some(thread);
        Ok(())
    }

    fn stop(&mut self) {
        // Dropping the thread posts WM_QUIT and joins it.
        self.thread = None;
    }
}

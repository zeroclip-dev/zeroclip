use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

use windows::Win32::UI::WindowsAndMessaging::WM_INPUT;
use zc_core::ports::{Emit, InputSource, OsEvent, SourceError};
use zc_win::message_thread::MessageThread;

use crate::{foreground, process_watch, raw_input};

/// Raw Input + foreground tracking on a dedicated message thread.
#[derive(Default)]
pub struct RawInputSource {
    thread: Option<MessageThread>,
    /// Kept from [`InputSource::start`] for the process watchers.
    events: Option<Emit<OsEvent>>,
    /// Pids with a live watcher, so a game refocused twice isn't watched twice.
    watched: Arc<Mutex<HashSet<u32>>>,
}

impl std::fmt::Debug for RawInputSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawInputSource")
            .field("thread", &self.thread)
            .field("started", &self.events.is_some())
            .field("watched", &self.watched)
            .finish()
    }
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
        self.events = Some(Arc::clone(&events));
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

    fn watch_process(&mut self, pid: u32) -> Result<(), SourceError> {
        let Some(events) = self.events.clone() else {
            return Err(SourceError::Other("input not started".into()));
        };
        // A poisoned set is still a valid set of pids; keep using it.
        if !self.watched.lock().unwrap_or_else(PoisonError::into_inner).insert(pid) {
            return Ok(());
        }
        let watched = Arc::clone(&self.watched);
        let emit: Emit<OsEvent> = Arc::new(move |event| {
            if let OsEvent::ProcessExited { pid, .. } = &event {
                watched.lock().unwrap_or_else(PoisonError::into_inner).remove(pid);
            }
            events(event);
        });
        process_watch::watch(pid, emit).inspect_err(|_| {
            self.watched.lock().unwrap_or_else(PoisonError::into_inner).remove(&pid);
        })
    }
}

//! The editor's player: decodes the clip with ffmpeg into raw frames and shows them in step with
//! a clock. Sound comes from `ffplay` when it sits beside ffmpeg.
//!
//! One ffmpeg process decodes at a time. Seeking or pausing kills it, so there is never a pile
//! of jobs. A reader thread turns each raw BGRA frame into a GPUI image and hands it over
//! through a small bounded channel, which also stops ffmpeg from racing ahead of the clock.
//!
//! ponytail: picture and sound start together but are not locked to one clock, so they can drift
//! a little on a long play. Add a shared clock if that shows.

use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};
use std::time::Instant;

use gpui::RenderImage;
use image::{Frame, RgbaImage};
use zc_core::Hns;
use zc_editor::ffmpeg::{Ffmpeg, frames_args};

/// Size every frame is made at. Small, because it is only a preview.
const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;
const FPS: u32 = 24;
/// Frames that may wait between the reader thread and the screen.
const BACKLOG: usize = 6;

/// A running ffmpeg decode, killed when dropped.
struct Decoder {
    child: Child,
    frames: Receiver<Arc<RenderImage>>,
    /// Clip time of the first frame, in seconds.
    start: f64,
    /// Frames taken from `frames` so far.
    taken: u32,
}

impl Decoder {
    fn start(
        ffmpeg: &Ffmpeg,
        path: &Path,
        at: Hns,
        max_frames: Option<u32>,
    ) -> std::io::Result<Self> {
        let mut cmd = Command::new(&ffmpeg.exe);
        cmd.args(frames_args(path, at, WIDTH, HEIGHT, FPS, max_frames))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        no_window(&mut cmd);
        let mut child = cmd.spawn()?;
        let Some(mut stdout) = child.stdout.take() else {
            let _ = child.kill();
            return Err(std::io::Error::other("ffmpeg gave no output pipe"));
        };
        let (tx, frames) = sync_channel(BACKLOG);
        std::thread::spawn(move || {
            let mut raw = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
            // Ends when ffmpeg exits or is killed (the read fails), or the player is gone.
            while stdout.read_exact(&mut raw).is_ok() {
                // ffmpeg's BGRA is the byte order GPUI wants, so the bytes go in as they are.
                let Some(buffer) = RgbaImage::from_raw(WIDTH, HEIGHT, raw.clone()) else {
                    break;
                };
                if tx.send(Arc::new(RenderImage::new(vec![Frame::new(buffer)]))).is_err() {
                    break;
                }
            }
        });
        Ok(Self { child, frames, start: at.as_secs_f64(), taken: 0 })
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Keeps a child process from opening a console window (release builds are GUI-subsystem).
/// <https://learn.microsoft.com/windows/win32/procthread/process-creation-flags>
fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// What the player shows and where it is in the clip.
#[derive(Default)]
pub struct Player {
    /// The current position in the clip.
    pub playhead: Hns,
    /// Whether the clock is running.
    pub playing: bool,
    /// The frame on screen.
    pub shown: Option<Arc<RenderImage>>,
    /// Frames that were on screen and can leave the GPU cache; the view drops them on render.
    pub spent: Vec<Arc<RenderImage>>,
    decoder: Option<Decoder>,
    /// Decoded frames and their clip times, waiting for the clock.
    queue: VecDeque<(f64, Arc<RenderImage>)>,
    audio: Option<Child>,
    /// When play began and the playhead then; the clock is the time since.
    started: Option<(Instant, Hns)>,
    /// Play stops when the playhead reaches this.
    limit: Hns,
}

impl Player {
    /// Whether the view still needs ticks: playing, or a frame is still on its way.
    pub fn busy(&self) -> bool {
        self.playing || self.decoder.is_some()
    }

    /// Forgets everything for a new clip.
    pub fn reset(&mut self) {
        self.halt();
        if let Some(old) = self.shown.take() {
            self.spent.push(old);
        }
        self.playhead = Hns::ZERO;
    }

    /// Stops decoding and sound. The picture and playhead stay.
    fn halt(&mut self) {
        self.decoder = None;
        self.queue.clear();
        if let Some(mut audio) = self.audio.take() {
            let _ = audio.kill();
            let _ = audio.wait();
        }
        self.playing = false;
        self.started = None;
    }

    /// Pauses where the clock is now.
    pub fn pause(&mut self) {
        self.halt();
    }

    /// Moves to `at` and shows the frame there.
    pub fn show_at(&mut self, ffmpeg: &Ffmpeg, path: &Path, at: Hns) {
        self.halt();
        self.playhead = at;
        self.decoder = Decoder::start(ffmpeg, path, at, Some(1)).ok();
    }

    /// Plays from `from` until `limit`.
    pub fn play(&mut self, ffmpeg: &Ffmpeg, path: &Path, from: Hns, limit: Hns) {
        self.halt();
        self.playhead = from;
        self.limit = limit;
        self.decoder = Decoder::start(ffmpeg, path, from, None).ok();
        if self.decoder.is_none() {
            return;
        }
        self.playing = true;
        self.started = Some((Instant::now(), from));
        self.audio = spawn_audio(ffmpeg, path, from, limit);
    }

    /// Advances the clock and picks the frame to show. Returns whether it is still busy.
    pub fn tick(&mut self) -> bool {
        let mut ended = false;
        if let Some(decoder) = self.decoder.as_mut() {
            loop {
                match decoder.frames.try_recv() {
                    Ok(image) => {
                        let time = decoder.start + f64::from(decoder.taken) / f64::from(FPS);
                        decoder.taken += 1;
                        self.queue.push_back((time, image));
                    }
                    Err(TryRecvError::Empty) => break,
                    // The decode is over and everything it made has been taken.
                    Err(TryRecvError::Disconnected) => {
                        ended = true;
                        break;
                    }
                }
            }
        }
        if let (true, Some((began, from))) = (self.playing, self.started) {
            self.playhead = from + Hns::from_secs_f64(began.elapsed().as_secs_f64());
            if self.playhead >= self.limit {
                self.playhead = self.limit;
                self.halt();
                return false;
            }
            let now = self.playhead.as_secs_f64();
            while self.queue.len() >= 2 && self.queue[1].0 <= now {
                self.queue.pop_front();
            }
            // Only a frame whose time has come goes on screen.
            if self.queue.front().is_some_and(|(time, _)| *time <= now)
                && let Some((_, image)) = self.queue.pop_front()
            {
                self.set_shown(image);
            }
            if ended && self.queue.is_empty() {
                self.halt();
                return false;
            }
        } else if let Some((_, image)) = self.queue.pop_back() {
            // Paused on a still: the one frame, whenever it arrives.
            self.queue.clear();
            self.set_shown(image);
        }
        if ended && !self.playing {
            self.decoder = None;
        }
        self.busy()
    }

    fn set_shown(&mut self, image: Arc<RenderImage>) {
        if let Some(old) = self.shown.replace(image) {
            self.spent.push(old);
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.halt();
    }
}

/// Starts `ffplay` with no window, for the sound of `from` to `limit`. `None` when ffplay isn't
/// beside ffmpeg: the picture then plays without sound.
fn spawn_audio(ffmpeg: &Ffmpeg, path: &Path, from: Hns, limit: Hns) -> Option<Child> {
    let ffplay = ffmpeg.exe.with_file_name(if cfg!(windows) { "ffplay.exe" } else { "ffplay" });
    if !ffplay.is_file() {
        return None;
    }
    let mut cmd = Command::new(ffplay);
    cmd.args(["-nodisp", "-vn", "-autoexit", "-loglevel", "quiet", "-ss"])
        .arg(format!("{:.3}", from.as_secs_f64()))
        .arg("-t")
        .arg(format!("{:.3}", (limit - from).as_secs_f64()))
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    no_window(&mut cmd);
    cmd.spawn().ok()
}

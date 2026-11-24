//! The editor's player: decodes the timeline's pieces with ffmpeg into raw frames and shows them
//! in step with a clock. Sound comes from `ffplay` when it sits beside ffmpeg.
//!
//! One ffmpeg process decodes at a time. Seeking, pausing or moving on to the next piece kills
//! it, so there is never a pile of jobs. A reader thread turns each raw BGRA frame into a GPUI
//! image and hands it over through a small bounded channel, which also stops ffmpeg from racing
//! ahead of the clock.
//!
//! ponytail: picture and sound start together but are not locked to one clock, so they can drift
//! a little on a long play. Add a shared clock if that shows.

use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};
use std::time::Instant;

use gpui::RenderImage;
use image::{Frame, RgbaImage};
use zc_core::Hns;
use zc_editor::ffmpeg::{Ffmpeg, frames_args};

/// Size every frame is made at. Small, because it is only a preview.
const WIDTH: u32 = 960;
const HEIGHT: u32 = 540;
const FPS: u32 = 24;
/// Frames that may wait between the reader thread and the screen.
const BACKLOG: usize = 6;

/// What to hear while a piece plays.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sound {
    /// Which audio track of the file (0 is Master).
    pub track: usize,
    pub gain_db: f64,
}

/// One piece to play: `[from, to)` of a file. `sound` is `None` for a muted piece.
#[derive(Clone, Debug)]
pub struct Item {
    pub path: PathBuf,
    pub from: Hns,
    pub to: Hns,
    pub sound: Option<Sound>,
}

/// A running ffmpeg decode, killed when dropped.
struct Decoder {
    child: Child,
    frames: Receiver<Arc<RenderImage>>,
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
        Ok(Self { child, frames, taken: 0 })
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

/// What the player shows and where it is on the timeline.
#[derive(Default)]
pub struct Player {
    /// The current position on the timeline.
    pub playhead: Hns,
    /// Whether the clock is running.
    pub playing: bool,
    /// The frame on screen.
    pub shown: Option<Arc<RenderImage>>,
    /// Frames that were on screen and can leave the GPU cache; the view drops them on render.
    pub spent: Vec<Arc<RenderImage>>,
    ffmpeg: Option<Ffmpeg>,
    decoder: Option<Decoder>,
    /// Decoded frames and their time since the piece began, waiting for the clock.
    queue: VecDeque<(f64, Arc<RenderImage>)>,
    audio: Option<Child>,
    /// The pieces still to play, the one playing first.
    items: VecDeque<Item>,
    /// Timeline time at which the current piece began.
    base: Hns,
    /// When the current piece began.
    started: Option<Instant>,
}

impl Player {
    /// Whether the view still needs ticks: playing, or a frame is still on its way.
    pub fn busy(&self) -> bool {
        self.playing || self.decoder.is_some()
    }

    /// Forgets everything for a new project.
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
        self.items.clear();
        if let Some(audio) = self.audio.take() {
            stop_audio(audio);
        }
        self.playing = false;
        self.started = None;
    }

    /// Pauses where the clock is now.
    pub fn pause(&mut self) {
        self.halt();
    }

    /// Moves to timeline time `at` and shows the frame of `path` at source time `src`.
    pub fn show_at(&mut self, ffmpeg: &Ffmpeg, path: &Path, src: Hns, at: Hns) {
        self.halt();
        self.playhead = at;
        self.decoder = Decoder::start(ffmpeg, path, src, Some(1)).ok();
    }

    /// Plays `items` in order, the first beginning at timeline time `at`.
    pub fn play(&mut self, ffmpeg: &Ffmpeg, items: Vec<Item>, at: Hns) {
        self.halt();
        self.ffmpeg = Some(ffmpeg.clone());
        self.items = items.into();
        self.playhead = at;
        self.base = at;
        self.playing = self.start_item();
    }

    /// Starts decoding and sound for the piece at the front. `false` when there is none.
    fn start_item(&mut self) -> bool {
        self.decoder = None;
        self.queue.clear();
        if let Some(audio) = self.audio.take() {
            stop_audio(audio);
        }
        let (Some(item), Some(ffmpeg)) = (self.items.front(), self.ffmpeg.as_ref()) else {
            return false;
        };
        self.decoder = Decoder::start(ffmpeg, &item.path, item.from, None).ok();
        if self.decoder.is_none() {
            return false;
        }
        self.audio = item.sound.and_then(|s| spawn_audio(ffmpeg, item, s));
        self.started = Some(Instant::now());
        true
    }

    /// Advances the clock and picks the frame to show. Returns whether it is still busy.
    pub fn tick(&mut self) -> bool {
        let mut ended = false;
        if let Some(decoder) = self.decoder.as_mut() {
            loop {
                match decoder.frames.try_recv() {
                    Ok(image) => {
                        let time = f64::from(decoder.taken) / f64::from(FPS);
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
        if self.playing {
            let Some(began) = self.started else { return self.busy() };
            let mut elapsed = began.elapsed().as_secs_f64();
            while let Some(item) = self.items.front() {
                let len = (item.to - item.from).as_secs_f64();
                if elapsed < len {
                    break;
                }
                // This piece is done: on to the next one, or stop at the end.
                self.base += item.to - item.from;
                self.items.pop_front();
                if self.items.is_empty() || !self.start_item() {
                    self.playhead = self.base;
                    self.halt();
                    return false;
                }
                ended = false;
                elapsed = 0.0;
            }
            self.playhead = self.base + Hns::from_secs_f64(elapsed);
            while self.queue.len() >= 2 && self.queue[1].0 <= elapsed {
                self.queue.pop_front();
            }
            // Only a frame whose time has come goes on screen.
            if self.queue.front().is_some_and(|(time, _)| *time <= elapsed)
                && let Some((_, image)) = self.queue.pop_front()
            {
                self.set_shown(image);
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

/// Stops an `ffplay` and everything it started. Killing only the process we spawned is not
/// enough when `ffplay.exe` is a launcher (Chocolatey and Scoop put one on PATH): the real player
/// is its child and would keep playing after a pause. `taskkill /T` takes the whole tree; it is
/// started here and waited for on a thread, so a pause never stalls the UI.
/// <https://learn.microsoft.com/windows-server/administration/windows-commands/taskkill>
fn stop_audio(mut audio: Child) {
    #[cfg(windows)]
    let tree = {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/F", "/T", "/PID", &audio.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        no_window(&mut cmd);
        cmd.spawn().ok()
    };
    std::thread::spawn(move || {
        #[cfg(windows)]
        if let Some(mut tree) = tree {
            let _ = tree.wait();
        }
        let _ = audio.kill();
        let _ = audio.wait();
    });
}

/// Starts `ffplay` with no window, for the sound of `item`'s track `sound.track`. `None` when
/// ffplay isn't beside ffmpeg: the picture then plays without sound.
fn spawn_audio(ffmpeg: &Ffmpeg, item: &Item, sound: Sound) -> Option<Child> {
    let ffplay = ffmpeg.exe.with_file_name(if cfg!(windows) { "ffplay.exe" } else { "ffplay" });
    if !ffplay.is_file() {
        return None;
    }
    let mut cmd = Command::new(ffplay);
    cmd.args(["-nodisp", "-vn", "-autoexit", "-loglevel", "quiet", "-ast"])
        .arg(format!("a:{}", sound.track))
        .arg("-ss")
        .arg(format!("{:.3}", item.from.as_secs_f64()))
        .arg("-t")
        .arg(format!("{:.3}", (item.to - item.from).as_secs_f64()));
    if sound.gain_db != 0.0 {
        cmd.arg("-af").arg(format!("volume={}dB", sound.gain_db));
    }
    cmd.arg(&item.path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    no_window(&mut cmd);
    cmd.spawn().ok()
}

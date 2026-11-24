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
#[derive(Clone, Debug, PartialEq)]
pub struct Sound {
    /// Which audio track of the file (0 is Master).
    pub track: usize,
    pub gain_db: f64,
    /// A volume line, counted from where the piece starts playing. It replaces `gain_db`.
    pub line: Option<Vec<zc_editor::ffmpeg::EnvelopePoint>>,
}

/// One piece to play: `[from, to)` of a file. `sound` is `None` for a muted piece.
#[derive(Clone, Debug)]
pub struct Item {
    pub path: PathBuf,
    pub from: Hns,
    pub to: Hns,
    pub sound: Option<Sound>,
}

/// One small picture of `path` at `at`, `w` x `h` pixels, for the timeline's thumbnails. `None`
/// when ffmpeg can't make it.
pub fn still(ffmpeg: &Ffmpeg, path: &Path, at: Hns, w: u32, h: u32) -> Option<Arc<RenderImage>> {
    let mut cmd = Command::new(&ffmpeg.exe);
    cmd.args(frames_args(path, at, w, h, 1, Some(1))).stdin(Stdio::null()).stderr(Stdio::null());
    no_window(&mut cmd);
    let out = cmd.output().ok()?;
    let buffer = RgbaImage::from_raw(w, h, out.stdout)?;
    Some(Arc::new(RenderImage::new(vec![Frame::new(buffer)])))
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
    /// How fast it plays: 1.0 is normal. Zero (a fresh player) reads as 1.0. Takes effect on the
    /// next `play`.
    pub speed: f64,
}

impl Player {
    /// The playback speed, 1.0 when none was set.
    fn rate(&self) -> f64 {
        if self.speed > 0.0 { self.speed } else { 1.0 }
    }

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
        let rate = self.rate();
        self.audio = item.sound.clone().and_then(|s| spawn_audio(ffmpeg, item, s, rate));
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
            let rate = self.rate();
            // Source seconds played: the wall clock, sped up or slowed down.
            let mut elapsed = began.elapsed().as_secs_f64() * rate;
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
            // ffplay gone long before its piece ended means no sound: say why, once.
            if let (Some(audio), Some(item)) = (self.audio.as_mut(), self.items.front())
                && elapsed < (item.to - item.from).as_secs_f64() - 1.0
                && let Ok(Some(status)) = audio.try_wait()
            {
                let said = std::fs::read_to_string(audio_log()).unwrap_or_default();
                tracing::warn!(%status, said = said.trim(), "the preview's sound stopped early");
                self.audio = None;
            }
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
fn spawn_audio(ffmpeg: &Ffmpeg, item: &Item, sound: Sound, rate: f64) -> Option<Child> {
    let ffplay = ffmpeg.exe.with_file_name(if cfg!(windows) { "ffplay.exe" } else { "ffplay" });
    if !ffplay.is_file() {
        return None;
    }
    let mut cmd = Command::new(ffplay);
    cmd.args(["-nodisp", "-vn", "-autoexit", "-loglevel", "error", "-ast"])
        .arg(format!("a:{}", sound.track))
        .arg("-ss")
        .arg(format!("{:.3}", item.from.as_secs_f64()))
        .arg("-t")
        .arg(format!("{:.3}", (item.to - item.from).as_secs_f64()));
    // The clock restarts at the seek, so a volume line counts from the start of what plays.
    let line = sound.line.as_deref().and_then(|l| zc_editor::ffmpeg::envelope_filter(l).ok());
    let mut filters = Vec::new();
    if let Some(filter) = line {
        filters.push(format!("asetpts=PTS-STARTPTS,{filter}"));
    } else if sound.gain_db != 0.0 {
        filters.push(format!("volume={}dB", sound.gain_db));
    }
    // atempo takes 0.5 to 2.0 a step, so faster or slower plays chain steps.
    let mut left = rate;
    while left > 2.0 {
        filters.push("atempo=2.0".to_owned());
        left /= 2.0;
    }
    while left < 0.5 {
        filters.push("atempo=0.5".to_owned());
        left /= 0.5;
    }
    if (left - 1.0).abs() > 1e-6 {
        filters.push(format!("atempo={left:.4}"));
    }
    if !filters.is_empty() {
        cmd.arg("-af").arg(filters.join(","));
    }
    // Its complaints go to a file, so a silent preview can be explained (see `tick`).
    let log = std::fs::File::create(audio_log()).map_or_else(|_| Stdio::null(), Stdio::from);
    cmd.arg(&item.path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(log);
    no_window(&mut cmd);
    cmd.spawn().ok()
}

/// Where `ffplay` writes what it says went wrong.
fn audio_log() -> PathBuf {
    std::env::temp_dir().join("zeroclip-ffplay.log")
}

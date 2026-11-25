//! The editor's player: decodes the timeline's pieces with ffmpeg into raw frames and shows them
//! in step with a clock. Sound comes from `ffplay` when it sits beside ffmpeg.
//!
//! One ffmpeg video decoder runs at a time. Each unmuted audio track is decoded by `ffmpeg` to raw
//! `f32le` stereo PCM, mixed in Rust via [`mix_audio_chunk`] (applying per-track mute, gain,
//! volume envelopes, silence ranges and limiting), scaled by the preview's master volume (see
//! [`scale_pcm_chunk`]) and piped straight into a single `ffplay` sink. Every piece takes this
//! path, so the volume can change while it plays. A gap piece has no decoder and no sound: the
//! picture is black and the clock runs on.
//!
//! Seeking, pausing or moving on to the next piece kills every decoder and `ffplay` tree on a
//! background thread so pausing never stalls the UI and never leaves background decode running.
//!
//! ponytail: picture and sound start together but are not locked to one clock, so they can drift
//! a little on a long play. Add a shared clock if that shows.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};
use std::time::Instant;

use gpui::RenderImage;
use image::{Frame, Rgba, RgbaImage};
use zc_core::Hns;
use zc_editor::ffmpeg::{EnvelopePoint, Ffmpeg, SILENT_DB, frames_args};

/// Size every frame is made at. Small, because it is only a preview.
const WIDTH: u32 = 960;
const HEIGHT: u32 = 540;
const FPS: u32 = 24;
/// Frames that may wait between the reader thread and the screen.
const BACKLOG: usize = 6;

/// Sample rate and channel count used by the multi-track PCM mixer (`48 kHz` stereo `f32le`).
pub const MIX_SAMPLE_RATE: u32 = 48_000;
/// Stereo channels for the multi-track PCM mixer.
pub const MIX_CHANNELS: usize = 2;
/// Audio frames processed per mixer iteration (~21 ms at 48 kHz: low latency, small buffers).
const MIX_CHUNK_FRAMES: usize = 1024;

/// The size of the black picture a gap shows. It is stretched to fit, so only the aspect matters.
const BLACK_SIZE: (u32, u32) = (16, 9);

/// A volume percentage kept in `0..=100`.
#[must_use]
pub fn clamp_volume(percent: u8) -> u8 {
    percent.min(100)
}

/// The linear gain for a `0..=100` volume and a mute flag. A quadratic taper (`(p / 100)^2`) so
/// 50% is about half as loud to the ear (-12 dB); 0% and muted are exact silence, 100% is unity.
#[must_use]
pub fn volume_gain(percent: u8, muted: bool) -> f32 {
    if muted {
        return 0.0;
    }
    let normalized = f32::from(clamp_volume(percent)) / 100.0;
    normalized * normalized
}

/// `percent` moved by `delta` points, kept in `0..=100`.
#[must_use]
pub fn step_volume(percent: u8, delta: i32) -> u8 {
    (i32::from(clamp_volume(percent)) + delta).clamp(0, 100) as u8
}

/// The speaker icon for a volume: crossed out at 0 or muted, one wave up to 50%, two above.
#[must_use]
pub fn volume_icon(percent: u8, muted: bool) -> &'static str {
    match if muted { 0 } else { clamp_volume(percent) } {
        0 => "volume-x",
        1..=50 => "volume-1",
        _ => "volume-2",
    }
}

/// Scales interleaved samples in place, ramping the gain in a straight line from `start` to `end`
/// across the chunk's frames, so a change of volume never clicks.
pub fn scale_pcm_chunk(samples: &mut [f32], start: f32, end: f32, channels: usize) {
    let frames = samples.len() / channels.max(1);
    if frames == 0 {
        return;
    }
    if start == end {
        if end != 1.0 {
            samples.iter_mut().for_each(|s| *s *= end);
        }
        return;
    }
    for (f, frame) in samples.chunks_exact_mut(channels).enumerate() {
        let gain = start + (end - start) * (f + 1) as f32 / frames as f32;
        frame.iter_mut().for_each(|s| *s *= gain);
    }
}

/// The preview's master gain, shared with the audio mixer thread. Starts at unity (1.0).
#[derive(Clone)]
pub struct MasterGain(Arc<AtomicU32>);

impl Default for MasterGain {
    fn default() -> Self {
        Self(Arc::new(AtomicU32::new(1.0_f32.to_bits())))
    }
}

impl MasterGain {
    /// Sets the gain; the mixer picks it up on its next chunk.
    pub fn set(&self, gain: f32) {
        self.0.store(gain.to_bits(), Ordering::Relaxed);
    }

    fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// What to hear for one audio track while a piece plays.
#[derive(Clone, Debug, PartialEq)]
pub struct Sound {
    /// Which audio track of the file (`0` is Master, `1..` are individual tracks).
    pub track: usize,
    /// When `true`, this track is muted and excluded from the mix.
    pub mute: bool,
    /// Flat gain in decibels when `line` is `None`.
    pub gain_db: f64,
    /// A volume line, counted from where the piece starts playing. It replaces `gain_db`.
    pub line: Option<Vec<EnvelopePoint>>,
    /// Explicit `[start, end)` silence ranges relative to where the piece starts playing.
    pub silence_ranges: Vec<(Hns, Hns)>,
}

impl Sound {
    /// Convenience constructor for a single unmuted track with optional gain and volume envelope.
    #[must_use]
    pub fn track(track: usize, gain_db: f64, line: Option<Vec<EnvelopePoint>>) -> Self {
        Self { track, mute: false, gain_db, line, silence_ranges: Vec::new() }
    }
}

/// One piece to play: `[from, to)` of a file, with the audio tracks to mix and an optional solo
/// track index (matching [`Sound::track`]).
#[derive(Clone, Debug)]
pub struct Item {
    /// Path to the media file.
    pub path: PathBuf,
    /// Start position in the source file.
    pub from: Hns,
    /// End position in the source file.
    pub to: Hns,
    /// Candidate audio tracks for this piece.
    pub sounds: Vec<Sound>,
    /// When `Some(track_id)`, only the entry with `sound.track == track_id` plays (solo).
    pub solo: Option<usize>,
}

impl Item {
    /// A black, silent gap `from..to` long: no file (an empty path), nothing decoded, only the
    /// clock runs. (Marked by the path so the other player, which builds items by hand, is
    /// untouched.)
    #[must_use]
    pub fn gap(from: Hns, to: Hns) -> Self {
        Self { path: PathBuf::new(), from, to, sounds: Vec::new(), solo: None }
    }

    fn is_gap(&self) -> bool {
        self.path.as_os_str().is_empty()
    }
}

/// Decides whether playback should pause when window activation changes, and remembers whether
/// playback was active when the window lost focus.
///
/// Losing OS focus while playing sets `*paused_on_blur = true` and returns `true` (caller must
/// pause). Regaining focus never auto-resumes (returns `false`).
#[must_use]
pub fn pause_on_focus_loss(
    was_playing: bool,
    window_active: bool,
    paused_on_blur: &mut bool,
) -> bool {
    if !window_active && was_playing {
        *paused_on_blur = true;
        true
    } else {
        false
    }
}

/// Picks which tracks from `sounds` are audible given `solo`:
/// - `solo == Some(id)`: only the matching track (if unmuted).
/// - `solo == None`: every unmuted track.
#[must_use]
pub fn active_sounds(sounds: &[Sound], solo: Option<usize>) -> Vec<Sound> {
    sounds.iter().filter(|s| !s.mute && solo.is_none_or(|id| s.track == id)).cloned().collect()
}

/// Evaluates the linear gain multiplier for `sound` at `time_secs` seconds into playback.
///
/// Respects `mute`, explicit `silence_ranges`, and envelope dips at or below [`SILENT_DB`]
/// (all returning exact `0.0`).
#[must_use]
pub fn sound_gain_at(sound: &Sound, time_secs: f64) -> f32 {
    if sound.mute || !time_secs.is_finite() {
        return 0.0;
    }
    let t_hns = Hns::from_secs_f64(time_secs.max(0.0));
    if sound.silence_ranges.iter().any(|&(start, end)| t_hns >= start && t_hns < end) {
        return 0.0;
    }
    let db = match sound.line.as_deref() {
        Some(points) if !points.is_empty() => eval_envelope_db(points, time_secs),
        _ => sound.gain_db,
    };
    if !db.is_finite() || db <= SILENT_DB {
        return 0.0;
    }
    10.0_f64.powf(db / 20.0) as f32
}

/// Piecewise-linear interpolation of an envelope in dB, flat before the first point and after
/// the last (matching `TrackEdit::db_at` and `zc_editor::ffmpeg::envelope_expr`).
fn eval_envelope_db(points: &[EnvelopePoint], time_secs: f64) -> f64 {
    if time_secs <= points[0].secs {
        return points[0].db;
    }
    for w in points.windows(2) {
        if time_secs <= w[1].secs {
            let dt = w[1].secs - w[0].secs;
            if dt <= 0.0 {
                return w[1].db;
            }
            let frac = (time_secs - w[0].secs) / dt;
            return w[0].db + (w[1].db - w[0].db) * frac;
        }
    }
    points[points.len() - 1].db
}

/// Clamps a summed sample into `[-1.0, 1.0]` and guards against non-finite samples.
///
/// Leaves audio below 0 dBFS untouched so unity-gain tracks keep exact levels, while preventing
/// clipping above full scale when multiple tracks sum together.
#[must_use]
pub fn limit_sample(sample: f32) -> f32 {
    if sample.is_finite() { sample.clamp(-1.0, 1.0) } else { 0.0 }
}

/// Mixes interleaved PCM streams into `out` (zero allocations).
///
/// Each entry in `tracks` pairs with the corresponding slice in `streams` (interleaved with
/// `channels` samples per frame). Respects `solo` (plays only the matching `track`), `mute`
/// (excluded), `gain_db`, volume `line` envelopes, and `silence_ranges`. Summed samples are
/// clamped to `[-1.0, 1.0]` via [`limit_sample`].
pub fn mix_audio_chunk(
    tracks: &[Sound],
    streams: &[&[f32]],
    solo: Option<usize>,
    start_frame: u64,
    sample_rate: u32,
    channels: usize,
    out: &mut [f32],
) {
    out.fill(0.0);
    if channels == 0 || sample_rate == 0 || out.is_empty() {
        return;
    }
    let frames = out.len() / channels;
    let inv_rate = 1.0 / f64::from(sample_rate);
    for (track, pcm) in tracks.iter().zip(streams.iter()) {
        if track.mute || solo.is_some_and(|id| track.track != id) {
            continue;
        }
        let avail_frames = (pcm.len() / channels).min(frames);
        for f in 0..avail_frames {
            let time_secs = (start_frame + f as u64) as f64 * inv_rate;
            let gain = sound_gain_at(track, time_secs);
            if gain == 0.0 {
                continue;
            }
            let base = f * channels;
            for ch in 0..channels {
                out[base + ch] += pcm[base + ch] * gain;
            }
        }
    }
    for sample in out.iter_mut() {
        *sample = limit_sample(*sample);
    }
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

/// The picture of a gap.
fn black_frame() -> Arc<RenderImage> {
    let black = RgbaImage::from_pixel(BLACK_SIZE.0, BLACK_SIZE.1, Rgba([0, 0, 0, 255]));
    Arc::new(RenderImage::new(vec![Frame::new(black)]))
}

/// A running ffmpeg decode, killed when dropped.
struct Decoder {
    /// `None` only once dropped, when it has been handed to a thread to reap.
    child: Option<Child>,
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
        Ok(Self { child: Some(child), frames, taken: 0 })
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // Killed here, so quitting never leaves ffmpeg running; only the wait (which can take a
        // moment while scrubbing drops one decoder per frame) is moved off the UI thread.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

/// Running preview audio: either a single `ffplay` process (single-track fast path) or a mixed
/// pipeline (`N` `ffmpeg` PCM decoders -> Rust mixer thread -> `ffplay` raw PCM sink).
struct AudioPlayback {
    /// The `ffplay` output process (always killed as a process tree via `taskkill /T`).
    sink: Child,
    /// Per-track `ffmpeg` decoder processes in multi-track mode; empty for direct single-track.
    decoders: Vec<Child>,
    /// Signals the mixer thread to stop immediately on pause/seek.
    stop: Arc<AtomicBool>,
}

impl AudioPlayback {
    fn try_wait_sink(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.sink.try_wait()
    }

    fn shutdown(self) {
        self.stop.store(true, Ordering::Relaxed);
        stop_audio_tree(self.sink, self.decoders);
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
    audio: Option<AudioPlayback>,
    /// The pieces still to play, the one playing first.
    items: VecDeque<Item>,
    /// Timeline time at which the current piece began.
    base: Hns,
    /// When the current piece began.
    started: Option<Instant>,
    /// How fast it plays: 1.0 is normal. Zero (a fresh player) reads as 1.0. Takes effect on the
    /// next `play`.
    pub speed: f64,
    /// The master volume of the preview.
    gain: MasterGain,
}

impl Player {
    /// Sets the preview's master volume (linear, `0.0..=1.0`). Heard at once, mid-playback.
    pub fn set_gain(&self, gain: f32) {
        self.gain.set(gain);
    }

    /// Shows black at timeline time `at` (the picture of a gap).
    pub fn show_black(&mut self, at: Hns) {
        self.halt();
        self.playhead = at;
        self.set_shown(black_frame());
    }

    /// The playback speed, 1.0 when none was set.
    fn rate(&self) -> f64 {
        if self.speed > 0.0 { self.speed } else { 1.0 }
    }

    /// Whether the view still needs ticks: playing, or a still frame is still on its way.
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
            audio.shutdown();
        }
        self.playing = false;
        self.started = None;
    }

    /// Pauses where the clock is now. Stops decoding and audio immediately.
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
            audio.shutdown();
        }
        let (Some(item), Some(ffmpeg)) = (self.items.front(), self.ffmpeg.as_ref()) else {
            return false;
        };
        if item.is_gap() {
            self.set_shown(black_frame());
            self.started = Some(Instant::now());
            return true;
        }
        self.decoder = Decoder::start(ffmpeg, &item.path, item.from, None).ok();
        if self.decoder.is_none() {
            return false;
        }
        let rate = self.rate();
        self.audio = spawn_audio_mix(ffmpeg, item, rate, &self.gain);
        self.started = Some(Instant::now());
        true
    }

    /// Advances the clock and picks the frame to show. Returns whether it is still busy.
    pub fn tick(&mut self) -> bool {
        if !self.busy() {
            return false;
        }
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
                && let Ok(Some(status)) = audio.try_wait_sink()
            {
                let said = std::fs::read_to_string(audio_log()).unwrap_or_default();
                tracing::warn!(%status, said = said.trim(), "the preview's sound stopped early");
                if let Some(audio) = self.audio.take() {
                    audio.shutdown();
                }
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
            // Paused on a still: once the single frame arrives, drop the decoder immediately so
            // nothing keeps decoding or ticking while paused.
            self.queue.clear();
            self.set_shown(image);
            self.decoder = None;
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

/// Stops `ffplay` (and its process tree, in case `ffplay.exe` is a Chocolatey/Scoop shim) plus
/// any per-track `ffmpeg` decoder processes on a background thread so pausing never stalls the UI.
/// <https://learn.microsoft.com/windows-server/administration/windows-commands/taskkill>
fn stop_audio_tree(mut sink: Child, mut decoders: Vec<Child>) {
    #[cfg(windows)]
    let tree = {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/F", "/T", "/PID", &sink.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        no_window(&mut cmd);
        cmd.spawn().ok()
    };
    // Killed on this thread (it is quick), so quitting right after never leaves a process behind;
    // only the waiting is left to a thread.
    for dec in &mut decoders {
        let _ = dec.kill();
    }
    let _ = sink.kill();
    std::thread::spawn(move || {
        for dec in &mut decoders {
            let _ = dec.wait();
        }
        #[cfg(windows)]
        if let Some(mut tree) = tree {
            let _ = tree.wait();
        }
        let _ = sink.wait();
    });
}

/// Starts preview audio for `item`: one `ffmpeg` PCM decoder per audible track, mixed in Rust
/// via [`mix_audio_chunk`], scaled by the master `gain` and piped into `ffplay`. `None` when no
/// track is audible or `ffplay` isn't beside ffmpeg.
fn spawn_audio_mix(
    ffmpeg: &Ffmpeg,
    item: &Item,
    rate: f64,
    gain: &MasterGain,
) -> Option<AudioPlayback> {
    let active = active_sounds(&item.sounds, item.solo);
    if active.is_empty() {
        return None;
    }
    let ffplay = ffmpeg.exe.with_file_name(if cfg!(windows) { "ffplay.exe" } else { "ffplay" });
    if !ffplay.is_file() {
        return None;
    }
    spawn_multi_track_mixer(ffmpeg, &ffplay, item, active, rate, gain.clone())
}

/// Appends chained `atempo` filters for playback speed `rate` (`atempo` accepts `0.5..=2.0`).
fn push_atempo_filters(filters: &mut Vec<String>, rate: f64) {
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
}

/// Spawns one `ffmpeg` PCM decoder per active track, mixes chunks on a dedicated thread with
/// [`mix_audio_chunk`], scales each chunk by `gain` (ramped from the last chunk's value, so a
/// volume change never clicks), and pipes interleaved stereo `f32le` to `ffplay`.
fn spawn_multi_track_mixer(
    ffmpeg: &Ffmpeg,
    ffplay: &Path,
    item: &Item,
    active: Vec<Sound>,
    rate: f64,
    gain: MasterGain,
) -> Option<AudioPlayback> {
    let duration_secs = (item.to - item.from).as_secs_f64();
    if duration_secs <= 0.0 {
        return None;
    }

    let mut decoders: Vec<Child> = Vec::with_capacity(active.len());
    let mut pipes: Vec<std::process::ChildStdout> = Vec::with_capacity(active.len());
    let mut tempo_filters = Vec::new();
    push_atempo_filters(&mut tempo_filters, rate);

    for sound in &active {
        let mut cmd = Command::new(&ffmpeg.exe);
        cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error"])
            .arg("-ss")
            .arg(format!("{:.6}", item.from.as_secs_f64()))
            // `-t` before `-i` counts source time; after it, output time, which `atempo` stretches
            // (a slow speed would cut the sound short).
            .arg("-t")
            .arg(format!("{duration_secs:.6}"))
            .arg("-i")
            .arg(zc_editor::ffmpeg::input_arg(&item.path))
            .args(["-vn", "-sn", "-map"])
            .arg(format!("0:a:{}", sound.track));
        if !tempo_filters.is_empty() {
            cmd.arg("-af").arg(tempo_filters.join(","));
        }
        cmd.args([
            "-ar",
            &MIX_SAMPLE_RATE.to_string(),
            "-ac",
            &MIX_CHANNELS.to_string(),
            "-f",
            "f32le",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
        no_window(&mut cmd);
        let Ok(mut child) = cmd.spawn() else {
            for mut d in decoders {
                let _ = d.kill();
                let _ = d.wait();
            }
            return None;
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            for mut d in decoders {
                let _ = d.kill();
                let _ = d.wait();
            }
            return None;
        };
        decoders.push(child);
        pipes.push(stdout);
    }

    let log = std::fs::File::create(audio_log()).map_or_else(|_| Stdio::null(), Stdio::from);
    let mut sink_cmd = Command::new(ffplay);
    // Low-latency raw PCM sink: `-probesize 32 -analyzeduration 0 -fflags nobuffer` avoids ffplay
    // buffering seconds of headerless PCM before starting audio output.
    sink_cmd
        .args([
            "-nodisp",
            "-autoexit",
            "-loglevel",
            "error",
            "-probesize",
            "32",
            "-analyzeduration",
            "0",
            "-fflags",
            "nobuffer",
            "-f",
            "f32le",
            "-sample_rate",
            &MIX_SAMPLE_RATE.to_string(),
            "-ch_layout",
            "stereo",
            "-i",
            "pipe:0",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(log);
    no_window(&mut sink_cmd);
    let Ok(mut sink) = sink_cmd.spawn() else {
        for mut d in decoders {
            let _ = d.kill();
            let _ = d.wait();
        }
        return None;
    };
    let Some(mut sink_in) = sink.stdin.take() else {
        stop_audio_tree(sink, decoders);
        return None;
    };

    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    // Effective sample rate for envelope timing accounts for playback speed.
    let timing_rate = ((f64::from(MIX_SAMPLE_RATE) / rate).round() as u32).max(1);

    let _ = std::thread::Builder::new().name("zc-audio-mix".into()).spawn(move || {
        let chunk_samples = MIX_CHUNK_FRAMES * MIX_CHANNELS;
        let chunk_bytes = chunk_samples * size_of::<f32>();
        let mut raw_bufs: Vec<Vec<u8>> = (0..pipes.len()).map(|_| vec![0u8; chunk_bytes]).collect();
        let mut pcm_bufs: Vec<Vec<f32>> =
            (0..pipes.len()).map(|_| vec![0.0_f32; chunk_samples]).collect();
        let mut lens: Vec<usize> = vec![0; pipes.len()];
        let mut eof: Vec<bool> = vec![false; pipes.len()];
        let mut mix_out = vec![0.0_f32; chunk_samples];
        let mut out_bytes = vec![0u8; chunk_bytes];
        let mut frame_cursor: u64 = 0;
        let mut last_gain = gain.get();

        while !stop_flag.load(Ordering::Relaxed) {
            let mut max_samples = 0usize;
            for idx in 0..pipes.len() {
                if eof[idx] {
                    lens[idx] = 0;
                    continue;
                }
                let got = read_chunk(&mut pipes[idx], &mut raw_bufs[idx]);
                if got == 0 {
                    eof[idx] = true;
                    lens[idx] = 0;
                    continue;
                }
                // Decode complete stereo frames only so channel alignment never drifts.
                let frame_bytes = MIX_CHANNELS * size_of::<f32>();
                let aligned_bytes = (got / frame_bytes) * frame_bytes;
                let samples = aligned_bytes / size_of::<f32>();
                for (s, b) in pcm_bufs[idx][..samples]
                    .iter_mut()
                    .zip(raw_bufs[idx][..aligned_bytes].as_chunks::<4>().0)
                {
                    *s = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                }
                lens[idx] = samples;
                max_samples = max_samples.max(samples);
            }
            if max_samples == 0 {
                break;
            }
            let streams: Vec<&[f32]> =
                pcm_bufs.iter().zip(&lens).map(|(buf, &len)| &buf[..len]).collect();
            mix_audio_chunk(
                &active,
                &streams,
                None,
                frame_cursor,
                timing_rate,
                MIX_CHANNELS,
                &mut mix_out[..max_samples],
            );
            let target = gain.get();
            scale_pcm_chunk(&mut mix_out[..max_samples], last_gain, target, MIX_CHANNELS);
            last_gain = target;
            let write_len = max_samples * size_of::<f32>();
            for (dst, &sample) in out_bytes[..write_len]
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(&mix_out[..max_samples])
            {
                dst.copy_from_slice(&sample.to_le_bytes());
            }
            if sink_in.write_all(&out_bytes[..write_len]).is_err() {
                break;
            }
            frame_cursor += (max_samples / MIX_CHANNELS) as u64;
        }
    });

    Some(AudioPlayback { sink, decoders, stop })
}

/// Reads up to `buf.len()` bytes from `reader`, looping across short pipe reads until `buf` is
/// full or EOF/error is reached.
fn read_chunk(reader: &mut impl Read, buf: &mut [u8]) -> usize {
    let mut total = 0;
    while total < buf.len() {
        match reader.read(&mut buf[total..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n,
        }
    }
    total
}

/// Where `ffplay` writes what it says went wrong.
fn audio_log() -> PathBuf {
    std::env::temp_dir().join("zeroclip-ffplay.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: usize, gain_db: f64, mute: bool) -> Sound {
        Sound { track: id, mute, gain_db, line: None, silence_ranges: Vec::new() }
    }

    #[test]
    fn mix_excludes_muted_tracks() {
        let tracks = [track(1, 0.0, false), track(2, 0.0, true)];
        let s1 = [0.25_f32, -0.25, 0.5, -0.5];
        let s2 = [0.6_f32, 0.6, 0.6, 0.6];
        let mut out = [0.0_f32; 4];

        mix_audio_chunk(&tracks, &[&s1, &s2], None, 0, 48_000, 2, &mut out);
        assert_eq!(out, [0.25, -0.25, 0.5, -0.5]);
    }

    #[test]
    fn mix_applies_per_track_gain_and_limits_peaks() {
        // Track 1 at -6.0206 dB (~0.5x), Track 2 at 0 dB (1.0x).
        let half_db = 20.0 * 0.5_f64.log10();
        let tracks = [track(1, half_db, false), track(2, 0.0, false)];
        let s1 = [0.4_f32, -0.4, 0.8, -0.8];
        let s2 = [0.3_f32, -0.3, 0.9, -0.9];
        let mut out = [0.0_f32; 4];

        mix_audio_chunk(&tracks, &[&s1, &s2], None, 0, 48_000, 2, &mut out);
        // Frame 0: 0.4 * 0.5 + 0.3 = 0.5
        assert!((out[0] - 0.5).abs() < 1e-5, "got {}", out[0]);
        assert!((out[1] + 0.5).abs() < 1e-5, "got {}", out[1]);
        // Frame 1: 0.8 * 0.5 + 0.9 = 1.3 -> clamped to 1.0
        assert_eq!(out[2], 1.0);
        assert_eq!(out[3], -1.0);
    }

    #[test]
    fn mix_respects_silence_ranges_and_envelope_dips() {
        // Explicit silence range [1.0 s, 2.0 s) on track 1, and envelope dip to SILENT_DB on track 2.
        let mut t1 = track(1, 0.0, false);
        t1.silence_ranges.push((Hns::from_secs(1), Hns::from_secs(2)));
        let t2 = Sound {
            track: 2,
            mute: false,
            gain_db: 0.0,
            line: Some(vec![
                EnvelopePoint { secs: 0.0, db: 0.0 },
                EnvelopePoint { secs: 0.5, db: 0.0 },
                EnvelopePoint { secs: 1.0, db: SILENT_DB },
                EnvelopePoint { secs: 2.0, db: SILENT_DB },
                EnvelopePoint { secs: 2.5, db: 0.0 },
            ]),
            silence_ranges: Vec::new(),
        };
        // 1 Hz sample rate: frame k is at t = k seconds.
        let s1 = [0.3_f32, 0.3, 0.3, 0.3, 0.3, 0.3];
        let s2 = [0.4_f32, 0.4, 0.4, 0.4, 0.4, 0.4];
        let mut out = [0.0_f32; 6];

        mix_audio_chunk(&[t1, t2], &[&s1, &s2], None, 0, 1, 2, &mut out);
        // t = 0 s: both audible (0.3 + 0.4 = 0.7)
        assert!((out[0] - 0.7).abs() < 1e-5, "t=0 got {}", out[0]);
        // t = 1 s: both silenced (0.0)
        assert_eq!(out[2], 0.0);
        assert_eq!(out[3], 0.0);
        // t = 2 s: t1 back on (range is half-open [1, 2)), t2 still silent at 2.0 s
        assert!((out[4] - 0.3).abs() < 1e-5, "t=2 got {}", out[4]);
    }

    #[test]
    fn mix_solo_plays_only_the_solo_track() {
        let tracks = [track(1, 0.0, false), track(2, 0.0, false), track(3, 0.0, false)];
        let s1 = [0.2_f32, 0.2];
        let s2 = [0.5_f32, -0.5];
        let s3 = [0.1_f32, 0.1];
        let mut out = [0.0_f32; 2];

        mix_audio_chunk(&tracks, &[&s1, &s2, &s3], Some(2), 0, 48_000, 2, &mut out);
        assert_eq!(out, [0.5, -0.5]);

        // Active selector agrees with mix_audio_chunk.
        let active = active_sounds(&tracks, Some(2));
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].track, 2);
    }

    #[test]
    fn focus_loss_pauses_and_remembers_without_auto_resuming() {
        let mut paused_on_blur = false;
        // Focus lost while playing -> pauses and remembers.
        assert!(pause_on_focus_loss(true, false, &mut paused_on_blur));
        assert!(paused_on_blur);
        // Regaining focus does NOT auto-resume.
        assert!(!pause_on_focus_loss(false, true, &mut paused_on_blur));
        assert!(paused_on_blur);
        // Losing focus while already paused -> no action and flag unchanged if fresh.
        let mut fresh = false;
        assert!(!pause_on_focus_loss(false, false, &mut fresh));
        assert!(!fresh);
    }

    #[test]
    fn volume_follows_a_squared_taper_and_mute_is_silence() {
        assert_eq!(volume_gain(100, false), 1.0);
        assert_eq!(volume_gain(0, false), 0.0);
        assert_eq!(volume_gain(50, false), 0.25);
        assert_eq!(volume_gain(100, true), 0.0);
        // Above 100 is clamped to unity.
        assert_eq!(volume_gain(250, false), 1.0);
        assert_eq!(step_volume(98, 5), 100);
        assert_eq!(step_volume(3, -5), 0);
        assert_eq!(step_volume(50, 5), 55);
    }

    #[test]
    fn the_speaker_icon_tracks_mute_and_level() {
        assert_eq!(volume_icon(100, true), "volume-x");
        assert_eq!(volume_icon(0, false), "volume-x");
        assert_eq!(volume_icon(1, false), "volume-1");
        assert_eq!(volume_icon(50, false), "volume-1");
        assert_eq!(volume_icon(51, false), "volume-2");
        assert_eq!(volume_icon(100, false), "volume-2");
    }

    #[test]
    fn a_volume_change_ramps_across_the_chunk_and_mute_silences_it() {
        let mut ramp = [1.0_f32; 8]; // 4 stereo frames
        scale_pcm_chunk(&mut ramp, 0.0, 1.0, 2);
        assert_eq!(ramp, [0.25, 0.25, 0.5, 0.5, 0.75, 0.75, 1.0, 1.0]);
        let mut muted = [0.5_f32, -0.5, 0.25, -0.25];
        scale_pcm_chunk(&mut muted, 0.0, 0.0, 2);
        assert_eq!(muted, [0.0; 4]);
        // Steady unity leaves the samples exactly as they were.
        let mut same = [0.3_f32, -0.3];
        scale_pcm_chunk(&mut same, 1.0, 1.0, 2);
        assert_eq!(same, [0.3, -0.3]);
        let mut empty: [f32; 0] = [];
        scale_pcm_chunk(&mut empty, 0.0, 1.0, 2);
    }

    #[test]
    fn the_master_gain_starts_at_unity_and_is_shared() {
        let gain = MasterGain::default();
        assert_eq!(gain.get(), 1.0);
        let other = gain.clone();
        other.set(0.25);
        assert_eq!(gain.get(), 0.25);
    }
}

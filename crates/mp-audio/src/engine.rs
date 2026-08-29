//! The audio engine: a command API for the UI, a worker thread that decodes,
//! and a real-time callback that plays.
//!
//! # Threading
//!
//! - **UI thread** sends [`Command`]s and reads [`Shared`] atomics. Never blocks.
//! - **Worker thread** owns the queue, decoder and resampler. Does everything
//!   expensive: opening files, building resamplers, seeking.
//! - **Audio callback** pops interleaved samples from a lock-free ring, applies
//!   smoothed gain, and writes to the device. Allocates nothing and never locks.
//!
//! The plan splits control and decode across two threads. M1 collapses them into
//! one: opening a file takes a few milliseconds, and the ring holds ~2 seconds of
//! audio, so a stall there is invisible. The split becomes worthwhile in M3, when
//! gapless playback needs the *next* track open while the current one is still
//! decoding.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam::channel::{Receiver, Sender, TryRecvError};
use mp_core::config::{CrossfadeCurve, Playback, RepeatMode, ReplayGainMode, ShuffleMode};

use crate::decode::TrackDecoder;
use crate::device::{self, Output};
use crate::dsp::chain::{Chain, Params};
use crate::dsp::crossfade;
use crate::dsp::eq::Bank;
use crate::dsp::limiter::Settings as LimiterSettings;
use crate::error::{AudioError, Result};
use crate::gapless::{Pending, Seam};
use crate::queue::{Queue, QueueEntry};
use crate::resample::Resampler;
use crate::shared::{Shared, Status, slider_to_gain};
use crate::viz::{self, Monitor};

/// Depth of the audio ring, in seconds.
///
/// Deep enough to ride out a file open or a scheduling hiccup, shallow enough
/// that a seek does not feel laggy (the ring is flushed on seek anyway).
const RING_SECONDS: f32 = 2.0;

/// How long the worker sleeps when it has nothing to do.
const IDLE_POLL: Duration = Duration::from_millis(4);

/// Time constant for volume smoothing. Long enough to remove zipper noise,
/// short enough to feel immediate.
/// Give up waiting for the callback to acknowledge a flush after this long.
///
/// Only reachable if the device has stopped calling us, in which case audio is
/// already broken and blocking the worker would make it worse.
const FLUSH_TIMEOUT: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// Public messages
// ---------------------------------------------------------------------------

/// Instructions from the UI to the engine.
#[derive(Debug, Clone)]
pub enum Command {
    /// Replace the queue and start playing at `start`.
    PlayNow {
        tracks: Vec<PathBuf>,
        start: usize,
    },
    /// Append to the queue without disturbing playback.
    Enqueue(Vec<PathBuf>),
    /// Insert directly after the current track.
    PlayNext(Vec<PathBuf>),
    /// Jump to a queue entry by its index.
    JumpTo(usize),
    /// Drop one queue entry by its index. Ignored for the playing track.
    Remove(usize),

    Play,
    Pause,
    TogglePlayPause,
    Stop,
    Next,
    Previous,

    /// Seek to a fraction of the current track, `0.0..=1.0`.
    SeekFraction(f32),

    /// Volume as a `0.0..=1.0` slider position, not a raw gain.
    SetVolume(f32),
    SetMuted(bool),
    SetRepeat(RepeatMode),
    SetShuffle(ShuffleMode),

    /// Replace the equalizer curve.
    ///
    /// Carries the settings rather than the coefficients: the worker holds the
    /// device's sample rate, and the same curve needs different coefficients at
    /// 44.1 kHz and 96 kHz.
    SetEqualizer {
        enabled: bool,
        gains_db: Vec<f32>,
        preamp_db: f32,
        limiter: bool,
    },
    /// Skip digital silence at the start and end of each track.
    SetTrimSilence(bool),
    /// Change the crossfade length and shape. Zero seconds switches it off.
    SetCrossfade {
        seconds: f32,
        curve: CrossfadeCurve,
    },
    /// Change how per-track level correction is applied.
    SetReplayGain {
        mode: ReplayGainMode,
        /// Applied to tracks that carry no ReplayGain tags at all.
        fallback_db: f32,
    },

    ClearQueue,
    /// Re-open the output device, e.g. after the user picks a different one.
    ReopenDevice {
        name: Option<String>,
        buffer_frames: Option<u32>,
    },
    Shutdown,
}

/// Things that happened, for the UI to react to.
///
/// Anything too large for an atomic travels this way, so the callback never
/// has to touch it.
#[derive(Debug, Clone)]
pub enum Event {
    /// A new track started. `duration` is `None` when the container omits it.
    TrackStarted {
        path: PathBuf,
        index: usize,
        duration: Option<Duration>,
    },
    /// The queue ran out and playback stopped.
    QueueFinished,
    /// A track could not be played; playback has already skipped past it.
    TrackFailed { path: PathBuf, reason: String },
    /// The queue changed: different tracks, a different order, or both.
    ///
    /// Carries the whole thing in play order rather than just a length,
    /// because that is the only way a caller can show what is coming next
    /// under shuffle — the order is the engine's, and it is rebuilt whenever a
    /// shuffled queue wraps.
    QueueChanged { entries: Vec<QueueEntry> },
    /// The output device was opened or re-opened.
    DeviceChanged { name: String, sample_rate: u32 },
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

/// The UI-facing handle to the engine.
///
/// Cloning is cheap and every method is non-blocking, so this can be called
/// freely from inside a repaint.
#[derive(Clone)]
pub struct AudioEngine {
    commands: Sender<Command>,
    events: Receiver<Event>,
    shared: Arc<Shared>,
    running: Arc<AtomicBool>,
    /// Where the worker leaves a visualiser feed for the UI to collect.
    ///
    /// A mutex rather than an atomic because a [`Monitor`] is a ring consumer,
    /// not a number — but it is only touched when a stream is built and when
    /// the UI picks it up, never from the callback.
    visualizer: Arc<Mutex<Option<Monitor>>>,
}

impl AudioEngine {
    /// Start the engine, opening the output device described by `settings`.
    pub fn new(settings: &Playback) -> Result<Self> {
        let output = device::open(settings.output_device.as_deref(), settings.buffer_frames)?;

        let shared = Arc::new(Shared::new());
        shared.set_device(output.sample_rate(), output.channels());
        shared.set_gain(slider_to_gain(settings.volume));
        shared.set_muted(settings.muted);

        let (command_tx, command_rx) = crossbeam::channel::unbounded();
        let (event_tx, event_rx) = crossbeam::channel::unbounded();

        let running = Arc::new(AtomicBool::new(true));

        let visualizer = Arc::new(Mutex::new(None));

        let worker = Worker {
            shared: Arc::clone(&shared),
            running: Arc::clone(&running),
            visualizer: Arc::clone(&visualizer),
            commands: command_rx,
            events: event_tx,
            queue: {
                let mut q = Queue::new();
                q.set_repeat(settings.repeat);
                q.set_shuffle(settings.shuffle);
                q
            },
            decoder: None,
            resampler: None,
            output: Some(output),
            stream: None,
            producer: None,
            pending_open: None,
            carry: Vec::new(),
            eq: EqSettings::default(),
            replay_gain_mode: settings.replay_gain,
            replay_gain_fallback_db: settings.replay_gain_fallback_db,
            track_gain_db: None,
            dsp: None,
            seam: Seam::new(),
            gapless: settings.gapless,
            trim_silence: settings.trim_silence,
            crossfade_secs: settings.crossfade_seconds,
            crossfade_curve: settings.crossfade_curve,
            fading: None,
            fade: Fade::default(),
            decoded_frames: 0,
            mix_scratch: Vec::new(),
            queue_revision: 0,
        };

        std::thread::Builder::new()
            .name("resonance-audio".into())
            .spawn(move || worker.run())
            .map_err(|err| {
                AudioError::DeviceInit(format!("could not start audio thread: {err}"))
            })?;

        Ok(Self {
            commands: command_tx,
            events: event_rx,
            shared,
            running,
            visualizer,
        })
    }

    /// Collect the visualiser feed, if one is waiting.
    ///
    /// Returns `Some` exactly once per output stream. The UI holds onto what it
    /// gets; when the device is reopened the old feed reports itself abandoned
    /// and a fresh one appears here to be collected.
    pub fn take_visualizer(&self) -> Option<Monitor> {
        self.visualizer.lock().ok()?.take()
    }

    /// Read-only view of playback state, safe to poll every frame.
    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    pub fn status(&self) -> Status {
        self.shared.status()
    }

    pub fn position_secs(&self) -> f64 {
        self.shared.position_secs()
    }

    pub fn duration_secs(&self) -> Option<f64> {
        self.shared.duration_secs()
    }

    pub fn progress(&self) -> f32 {
        self.shared.progress()
    }

    pub fn xruns(&self) -> u64 {
        self.shared.xruns()
    }

    pub fn dropped(&self) -> u64 {
        self.shared.dropped()
    }

    /// Send a command. Dropped silently if the engine has shut down, since a
    /// dead engine is not something a button press should have to handle.
    pub fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            tracing::warn!("audio engine is not running; command dropped");
        }
    }

    /// Drain everything that has happened since the last call.
    pub fn poll_events(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }

    // Convenience wrappers, so callers do not have to name `Command`.

    pub fn play_now(&self, tracks: Vec<PathBuf>, start: usize) {
        self.send(Command::PlayNow { tracks, start });
    }

    pub fn toggle_play_pause(&self) {
        self.send(Command::TogglePlayPause);
    }

    pub fn next(&self) {
        self.send(Command::Next);
    }

    pub fn previous(&self) {
        self.send(Command::Previous);
    }

    pub fn seek_fraction(&self, fraction: f32) {
        self.send(Command::SeekFraction(fraction));
    }

    pub fn set_volume(&self, slider: f32) {
        self.send(Command::SetVolume(slider));
    }

    pub fn set_muted(&self, muted: bool) {
        self.send(Command::SetMuted(muted));
    }

    pub fn set_repeat(&self, mode: RepeatMode) {
        self.send(Command::SetRepeat(mode));
    }

    pub fn set_shuffle(&self, mode: ShuffleMode) {
        self.send(Command::SetShuffle(mode));
    }

    /// Apply an equalizer curve.
    ///
    /// Takes the settings, not coefficients: the worker knows the device's
    /// sample rate, and the same curve needs different coefficients at 44.1 kHz
    /// and 96 kHz.
    pub fn set_equalizer(&self, equalizer: &mp_core::config::Equalizer) {
        self.send(Command::SetEqualizer {
            enabled: equalizer.enabled,
            gains_db: equalizer.gains_db.clone(),
            preamp_db: equalizer.preamp_db,
            limiter: equalizer.limiter,
        });
    }

    /// Set the crossfade length and shape.
    pub fn set_crossfade(&self, seconds: f32, curve: CrossfadeCurve) {
        self.send(Command::SetCrossfade { seconds, curve });
    }

    /// Skip digital silence at the start and end of each track.
    pub fn set_trim_silence(&self, trim: bool) {
        self.send(Command::SetTrimSilence(trim));
    }

    pub fn set_replay_gain(&self, mode: ReplayGainMode, fallback_db: f32) {
        self.send(Command::SetReplayGain { mode, fallback_db });
    }

    /// Whether the limiter is currently reducing gain, for the clip indicator.
    pub fn is_limiting(&self) -> bool {
        self.shared.limiting()
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        // Only the last handle shuts the worker down.
        if Arc::strong_count(&self.running) <= 1 {
            self.running.store(false, Ordering::Release);
            let _ = self.commands.send(Command::Shutdown);
        }
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

/// How many parameter updates may be in flight to the callback.
///
/// Deep enough that dragging a slider never blocks the control thread, shallow
/// enough that a stalled callback cannot accumulate a visible backlog.
const DSP_QUEUE_DEPTH: usize = 16;

struct Worker {
    shared: Arc<Shared>,
    running: Arc<AtomicBool>,
    /// Where a freshly built visualiser feed is left for the UI.
    visualizer: Arc<Mutex<Option<Monitor>>>,
    commands: Receiver<Command>,
    events: Sender<Event>,

    queue: Queue,
    decoder: Option<TrackDecoder>,
    resampler: Option<Resampler>,

    output: Option<Output>,
    /// Held to keep the device alive; `cpal` stops the stream when dropped.
    stream: Option<cpal::Stream>,
    producer: Option<rtrb::Producer<f32>>,

    /// A track waiting to be opened, deferred so the command loop stays
    /// responsive while a slow file is loading.
    pending_open: Option<PathBuf>,

    /// Converted samples produced but not yet accepted by the ring.
    ///
    /// The ring can refuse a write when it is nearly full. Discarding the
    /// remainder punches a hole in the waveform, which is heard as a click at
    /// any volume, so it is held here and pushed before anything new is pulled.
    carry: Vec<f32>,

    /// The equalizer settings as the user set them, kept in their own terms so
    /// coefficients can be rebuilt when the device's sample rate changes.
    eq: EqSettings,
    replay_gain_mode: ReplayGainMode,
    replay_gain_fallback_db: f32,
    /// Level correction for the track currently open.
    track_gain_db: Option<f32>,

    /// Ships freshly computed DSP parameters to the callback.
    dsp: Option<rtrb::Producer<Params>>,

    /// Track boundaries sitting inside the buffered audio.
    ///
    /// Non-empty only while gapless playback is running ahead of the speakers.
    seam: Seam,
    /// Whether to join tracks without flushing the ring.
    gapless: bool,
    /// Whether to drop digital silence at track boundaries.
    trim_silence: bool,

    /// Length of the crossfade, in seconds. Zero switches it off.
    crossfade_secs: f32,
    crossfade_curve: CrossfadeCurve,
    /// The track being faded out, while one is.
    fading: Option<Fading>,
    /// Progress through the current fade.
    fade: Fade,
    /// Device frames produced from the current track.
    ///
    /// Counted on the decode side, not the playback side. Decoding runs up to
    /// a ring ahead, so a fade triggered from the playback position would be
    /// attenuating audio that left the buffer seconds ago.
    decoded_frames: u64,
    /// Scratch for one mixed block, so the fade does not allocate per block.
    mix_scratch: Vec<f32>,

    /// The queue revision last published to the UI.
    queue_revision: u64,
}

/// The outgoing track during a crossfade.
///
/// Kept whole — decoder, resampler and the samples already pulled out of it —
/// because a fade has to keep decoding the track it is fading *out*, not just
/// attenuate what happened to be buffered when it started.
struct Fading {
    decoder: Option<TrackDecoder>,
    resampler: Resampler,
    /// Interleaved samples pulled but not yet mixed.
    ready: std::collections::VecDeque<f32>,
}

impl Fading {
    fn new(decoder: TrackDecoder, resampler: Resampler) -> Self {
        Self {
            decoder: Some(decoder),
            resampler,
            ready: std::collections::VecDeque::new(),
        }
    }

    /// Fill `out` with the outgoing track, zero-padding once it runs dry.
    ///
    /// Padding with silence rather than stopping the fade is deliberate: the
    /// incoming track has to keep following the curve up to full level on
    /// schedule, whether or not the outgoing one lasted long enough to meet it.
    fn take(&mut self, out: &mut [f32]) {
        while self.ready.len() < out.len() {
            if let Some(block) = self.resampler.pull() {
                self.ready.extend(block.iter().copied());
                continue;
            }

            let Some(decoder) = self.decoder.as_mut() else {
                break;
            };

            match decoder.next_chunk() {
                Ok(Some(chunk)) => {
                    self.resampler.push(&chunk.planes, chunk.frames);
                    decoder.recycle(chunk);
                }
                Ok(None) => {
                    if let Some(tail) = self.resampler.drain() {
                        self.ready.extend(tail.iter().copied());
                    }
                    self.decoder = None;
                    break;
                }
                Err(err) => {
                    // The track being faded out is on its way to silence
                    // anyway; a read error near its end is not worth
                    // interrupting the incoming track for.
                    tracing::debug!("crossfade tail ended early: {err}");
                    self.decoder = None;
                    break;
                }
            }
        }

        for slot in out.iter_mut() {
            *slot = self.ready.pop_front().unwrap_or(0.0);
        }
    }
}

/// How far through a crossfade the stream is, in device frames.
#[derive(Debug, Clone, Copy, Default)]
struct Fade {
    frame: u64,
    total: u64,
}

/// The equalizer as the user configured it, before it becomes coefficients.
#[derive(Debug, Clone)]
struct EqSettings {
    enabled: bool,
    gains_db: Vec<f32>,
    preamp_db: f32,
    limiter: bool,
}

impl Default for EqSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            gains_db: vec![0.0; crate::dsp::eq::BAND_COUNT],
            preamp_db: 0.0,
            limiter: true,
        }
    }
}

impl Worker {
    fn run(mut self) {
        if let Err(err) = self.start_stream() {
            tracing::error!("could not start the audio output: {err:#}");
            return;
        }

        while self.running.load(Ordering::Acquire) {
            match self.pump_commands() {
                ControlFlow::Continue => {}
                ControlFlow::Shutdown => break,
            }

            if let Some(path) = self.pending_open.take() {
                self.open_track(&path);
            }

            self.advance_across_seams();
            self.sync_queue();

            let did_work = self.pump_audio();

            // Only sleep when there was nothing useful to do, so the ring
            // refills as fast as the device drains it.
            if !did_work {
                std::thread::sleep(IDLE_POLL);
            }
        }

        tracing::debug!("audio worker stopped");
    }

    /// Publish the queue if it has changed since it was last sent.
    ///
    /// The single place a `QueueChanged` is emitted. Every mutation bumps the
    /// queue's revision, so nothing has to remember to announce itself — which
    /// is what keeps the reshuffle-on-wrap case, the one with no command behind
    /// it, from silently going unreported.
    fn sync_queue(&mut self) {
        let revision = self.queue.revision();
        if revision == self.queue_revision {
            return;
        }
        self.queue_revision = revision;

        let entries = self.queue.entries();
        self.emit(Event::QueueChanged { entries });
    }

    /// Open the device and wire up the ring buffer and callback.
    fn start_stream(&mut self) -> Result<()> {
        let output = self.output.take().ok_or(AudioError::NoOutputDevice)?;

        let channels = output.channels();
        let rate = output.sample_rate();
        let capacity = (rate as f32 * RING_SECONDS) as usize * channels;

        let (producer, mut consumer) = rtrb::RingBuffer::<f32>::new(capacity);
        self.producer = Some(producer);

        self.shared.set_device(rate, channels);

        // Parameters travel to the callback through their own lock-free queue.
        // Only the newest matters, so a slow callback simply skips the ones it
        // missed rather than working through a backlog.
        let (dsp_tx, mut dsp_rx) = rtrb::RingBuffer::<Params>::new(DSP_QUEUE_DEPTH);
        self.dsp = Some(dsp_tx);

        let shared = Arc::clone(&self.shared);

        let mut chain = Chain::new();
        chain.set_params(self.current_params(rate as f32));
        chain.prime_volume(shared.effective_gain());

        // The visualiser feed belongs to this stream. Building a new one per
        // stream is what makes a device change work: the UI's old monitor
        // reports itself abandoned, it collects the replacement, and the
        // spectrum picks up at the new sample rate instead of silently
        // analysing a dead ring.
        let (tap, monitor) = viz::channel();
        chain.set_tap(Some(tap));
        if let Ok(mut slot) = self.visualizer.lock() {
            *slot = Some(monitor);
        }

        let stream = device::build_stream(&output, move |out: &mut [f32]| {
            // 1. Adopt any new parameters. Drained to the newest: an older set
            //    on the way is already obsolete.
            let mut latest = None;
            while let Ok(params) = dsp_rx.pop() {
                latest = Some(params);
            }
            if let Some(params) = latest {
                chain.set_params(params);
            }

            // 2. Honour a pending flush before anything else, so a seek does
            //    not play a burst of stale audio first. The filter tails and
            //    limiter reduction belong to the audio being discarded, so they
            //    go with it.
            if let Some(sequence) = shared.flush_pending() {
                while consumer.pop().is_ok() {}
                chain.reset();
                shared.acknowledge_flush(sequence);
            }

            let running = shared.status().is_playing() && !shared.priming();
            if running {
                chain.fade_in();
            } else {
                chain.fade_out();
            }

            // Fully faded out and not playing: nothing to render. `out` is
            // already zeroed by the caller.
            if !running && chain.is_silent() {
                return;
            }

            // 3. Pull raw samples. The DSP chain runs over them afterwards, in
            //    one pass over contiguous frames, rather than per-pop.
            let mut written = 0;
            while written < out.len() {
                match consumer.pop() {
                    Ok(sample) => {
                        out[written] = sample;
                        written += 1;
                    }
                    Err(_) => break,
                }
            }

            // Only whole frames can be processed; a partial one would put the
            // channels out of step with the filter state.
            let frames = written / channels;
            let usable = frames * channels;

            if usable > 0 {
                chain.process(&mut out[..usable], channels, shared.effective_gain());
                shared.advance_position(frames as u64);
                shared.set_limiting(chain.limiter().is_reducing());
            }

            // Running dry mid-track is an underrun worth counting. Running dry
            // at the end of a track is simply the track ending, and running dry
            // while fading out after a pause is expected.
            if written < out.len() && running && !shared.end_of_track() {
                shared.note_xrun();
            }
        })?;

        let name = output.name.clone();
        self.stream = Some(stream);
        self.output = Some(output);

        self.emit(Event::DeviceChanged {
            name,
            sample_rate: rate,
        });

        Ok(())
    }

    /// Build the DSP parameters for the current settings and device rate.
    ///
    /// This is where every transcendental function in the signal path lives:
    /// forty for the equalizer coefficients, a handful for the smoothing
    /// constants. The callback receives the results and does arithmetic only.
    fn current_params(&self, sample_rate: f32) -> Params {
        let mut params = Params::for_rate(sample_rate);

        params.bank = Bank::new(
            &self.eq.gains_db,
            self.eq.preamp_db,
            sample_rate,
            self.eq.enabled,
        );

        params.limiter = LimiterSettings::new(
            self.eq.limiter,
            LimiterSettings::DEFAULT_CEILING_DB,
            sample_rate,
        );

        params.replay_gain = crate::dsp::eq::db_to_linear(self.replay_gain_db());
        params
    }

    /// Level correction for the track currently open, in decibels.
    ///
    /// Falls back to the configured value only for tracks that carry no tags
    /// at all - a scanned file's own measurement always beats a blanket guess.
    fn replay_gain_db(&self) -> f32 {
        if self.replay_gain_mode == ReplayGainMode::Off {
            return 0.0;
        }
        self.track_gain_db.unwrap_or(self.replay_gain_fallback_db)
    }

    /// Recompute and ship the DSP parameters.
    ///
    /// Cheap enough to call on every settings change: the queue keeps only what
    /// the callback has not yet taken, so dragging a slider costs one
    /// coefficient build per UI frame rather than one per sample.
    fn publish_params(&mut self) {
        let rate = self.shared.device_rate();
        if rate == 0 {
            return;
        }

        let params = self.current_params(rate as f32);
        if let Some(dsp) = self.dsp.as_mut() {
            // A full queue means the callback has not run in a while. Dropping
            // this update is right: a newer one will follow, and blocking here
            // would stall the control thread on the audio thread.
            if dsp.is_full() {
                tracing::debug!("dsp parameter queue full; dropping an update");
                return;
            }
            let _ = dsp.push(params);
        }
    }

    // -- commands ----------------------------------------------------------

    fn pump_commands(&mut self) -> ControlFlow {
        loop {
            match self.commands.try_recv() {
                Ok(Command::Shutdown) => return ControlFlow::Shutdown,
                Ok(command) => self.handle(command),
                Err(TryRecvError::Empty) => return ControlFlow::Continue,
                Err(TryRecvError::Disconnected) => return ControlFlow::Shutdown,
            }
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::PlayNow { tracks, start } => {
                self.queue.replace(tracks, start);
                self.load_current();
            }

            Command::Enqueue(tracks) => {
                let was_empty = self.queue.is_empty();
                self.queue.extend(tracks);
                // Adding to an empty, idle queue should start playing it.
                if was_empty {
                    self.load_current();
                }
            }

            Command::PlayNext(tracks) => {
                self.queue.play_next(tracks);
            }

            Command::JumpTo(index) => {
                if self.queue.jump_to(index).is_some() {
                    self.load_current();
                }
            }

            Command::Remove(index) => {
                self.queue.remove(index);
            }

            Command::Play => {
                if self.decoder.is_some() {
                    self.shared.set_status(Status::Playing);
                } else {
                    self.load_current();
                }
            }

            Command::Pause => {
                if self.shared.status() == Status::Playing {
                    self.shared.set_status(Status::Paused);
                }
            }

            Command::TogglePlayPause => match self.shared.status() {
                Status::Playing => self.shared.set_status(Status::Paused),
                Status::Paused => self.shared.set_status(Status::Playing),
                Status::Stopped => self.load_current(),
            },

            Command::Stop => self.stop(),

            Command::Next => {
                let next = self.queue.next().map(Path::to_path_buf);
                match next {
                    Some(path) => self.begin(path),
                    None => self.finish_queue(),
                }
            }

            Command::Previous => {
                // Pressing previous a little way into a track restarts it,
                // which is what every other player does.
                if self.shared.position_secs() > 3.0 {
                    self.seek_to(Duration::ZERO);
                    return;
                }
                let previous = self.queue.previous().map(Path::to_path_buf);
                if let Some(path) = previous {
                    self.begin(path);
                }
            }

            Command::SeekFraction(fraction) => {
                if let Some(total) = self.shared.duration_secs() {
                    let target = (f64::from(fraction.clamp(0.0, 1.0)) * total).max(0.0);
                    self.seek_to(Duration::from_secs_f64(target));
                }
            }

            Command::SetVolume(slider) => self.shared.set_gain(slider_to_gain(slider)),
            Command::SetMuted(muted) => self.shared.set_muted(muted),
            Command::SetRepeat(mode) => self.queue.set_repeat(mode),
            Command::SetShuffle(mode) => self.queue.set_shuffle(mode),

            Command::SetCrossfade { seconds, curve } => {
                self.crossfade_secs = seconds.max(0.0);
                self.crossfade_curve = curve;
                // A fade already running is left to finish on the settings it
                // started with; changing the curve underneath it would step
                // the gain.
            }

            Command::SetTrimSilence(trim) => {
                self.trim_silence = trim;
                // Applies from the next track: retrimming the one already
                // decoding would jump the playhead.
                if let Some(decoder) = &mut self.decoder {
                    decoder.set_trim_silence(trim);
                }
            }

            Command::SetEqualizer {
                enabled,
                gains_db,
                preamp_db,
                limiter,
            } => {
                self.eq = EqSettings {
                    enabled,
                    gains_db,
                    preamp_db,
                    limiter,
                };
                self.publish_params();
            }

            Command::SetReplayGain { mode, fallback_db } => {
                self.replay_gain_mode = mode;
                self.replay_gain_fallback_db = fallback_db;
                self.publish_params();
            }

            Command::ClearQueue => {
                self.queue.clear();
                self.stop();
            }

            Command::ReopenDevice {
                name,
                buffer_frames,
            } => self.reopen_device(name, buffer_frames),

            Command::Shutdown => {}
        }
    }

    // -- playback ----------------------------------------------------------

    /// Open whatever the queue currently points at.
    fn load_current(&mut self) {
        match self.queue.current().map(Path::to_path_buf) {
            Some(path) => self.begin(path),
            None => self.finish_queue(),
        }
    }

    /// Switch to `path`, flushing whatever is still queued for the old track.
    ///
    /// Priming is raised *before* the flush, not in `open_track`: opening the
    /// file happens on the next worker iteration, and without this the callback
    /// would spend that gap draining a ring we just deliberately emptied and
    /// counting it as an underrun.
    fn begin(&mut self, path: PathBuf) {
        self.shared.set_priming(true);
        self.carry.clear();
        // Whatever was fading belonged to audio that is about to be discarded.
        self.fading = None;
        self.decoded_frames = 0;
        // Anything buffered is about to be thrown away, and so is every track
        // boundary inside it.
        self.seam.clear();
        self.discard_buffered();
        self.pending_open = Some(path);
    }

    /// Open a file and start decoding it.
    ///
    /// A failure here skips the track rather than stopping playback: one
    /// unreadable file in a folder should not end the listening session.
    fn open_track(&mut self, path: &Path) {
        match TrackDecoder::open(path) {
            Ok(mut decoder) => {
                decoder.set_trim_silence(self.trim_silence);
                let rate = decoder.sample_rate();
                let channels = self.shared.device_channels();
                let duration = decoder.duration();

                // The file's own level correction, read while it was opened.
                // Published before playback starts so the first sample already
                // has the right gain rather than sliding into it.
                let replay_gain = decoder.replay_gain();

                match Resampler::new(rate, self.shared.device_rate(), channels) {
                    Ok(resampler) => {
                        self.shared.reset_for_new_track(duration);
                        self.resampler = Some(resampler);
                        self.decoder = Some(decoder);
                        self.decoded_frames = 0;

                        self.track_gain_db = replay_gain.for_mode(self.replay_gain_mode);
                        self.publish_params();

                        self.shared.set_status(Status::Playing);

                        self.emit(Event::TrackStarted {
                            path: path.to_path_buf(),
                            index: self.queue.current_index().unwrap_or(0),
                            duration,
                        });
                    }
                    Err(err) => self.skip_failed(path, &err.to_string()),
                }
            }
            Err(err) => self.skip_failed(path, &err.to_string()),
        }
    }

    /// Report a bad track and move on to the next one.
    fn skip_failed(&mut self, path: &Path, reason: &str) {
        tracing::warn!("skipping {}: {reason}", path.display());

        self.emit(Event::TrackFailed {
            path: path.to_path_buf(),
            reason: reason.to_owned(),
        });

        match self.queue.next().map(Path::to_path_buf) {
            Some(next) => self.pending_open = Some(next),
            None => self.finish_queue(),
        }
    }

    /// Decode and push as much as the ring will take.
    ///
    /// Returns whether anything was done, so the caller knows if it can sleep.
    fn pump_audio(&mut self) -> bool {
        if self.shared.status() != Status::Playing {
            return false;
        }

        // Always retry the carry-over first, even once decoding has finished:
        // the tail of the track can be sitting in it.
        let mut worked = self.flush_carry();

        // The decoder finished a while ago; wait for the buffered tail to
        // actually reach the speakers before moving on.
        if self.decoder.is_none() {
            if self.carry.is_empty() && self.shared.track_fully_played() {
                self.advance_after_playback();
                return true;
            }
            return worked;
        }

        self.maybe_start_crossfade();

        let mut failure = None;
        let mut reached_eof = false;

        {
            // Disjoint borrows: the loop needs the producer, the resampler and
            // the carry buffer at the same time.
            let Self {
                shared,
                decoder,
                resampler,
                producer,
                carry,
                fading,
                fade,
                crossfade_curve,
                decoded_frames,
                mix_scratch,
                ..
            } = self;

            let (Some(producer), Some(resampler)) = (producer.as_mut(), resampler.as_mut()) else {
                return worked;
            };

            // Refill in bounded steps so commands are still serviced promptly
            // while a large ring fills from empty.
            for _ in 0..8 {
                // Anything left over must clear before more is produced, or the
                // ordering of the stream would break.
                if !carry.is_empty() {
                    break;
                }

                // Only pull when a whole worst-case block is guaranteed to fit.
                // Pulling into insufficient space is what produced the dropped
                // samples this buffer now prevents.
                if producer.slots() < resampler.max_output_samples() {
                    break;
                }

                if let Some(block) = resampler.pull() {
                    let channels = shared.device_channels().max(1);
                    *decoded_frames += (block.len() / channels) as u64;

                    match fading.as_mut() {
                        // Blend the outgoing track over this block. The
                        // outgoing stream is pulled to exactly this length and
                        // zero-padded if it has already ended, so the incoming
                        // one reaches full level on schedule either way.
                        Some(out) if fade.frame < fade.total => {
                            mix_scratch.clear();
                            mix_scratch.resize(block.len(), 0.0);
                            out.take(mix_scratch.as_mut_slice());

                            crossfade::mix(
                                mix_scratch,
                                block,
                                channels,
                                *crossfade_curve,
                                fade.frame,
                                fade.total,
                            );
                            fade.frame += (block.len() / channels) as u64;

                            Self::push_block(producer, shared, carry, mix_scratch);
                        }
                        _ => {
                            // Either no fade, or one that has run its course.
                            *fading = None;
                            Self::push_block(producer, shared, carry, block);
                        }
                    }

                    worked = true;
                    continue;
                }

                let Some(decoder) = decoder.as_mut() else {
                    break;
                };

                match decoder.next_chunk() {
                    Ok(Some(chunk)) => {
                        resampler.push(&chunk.planes, chunk.frames);
                        decoder.recycle(chunk);
                        worked = true;
                    }
                    Ok(None) => {
                        // End of file: flush the tail the resampler still holds,
                        // then let the buffered audio play out.
                        if let Some(tail) = resampler.drain() {
                            Self::push_block(producer, shared, carry, tail);
                        }
                        reached_eof = true;
                        worked = true;
                        break;
                    }
                    Err(err) => {
                        failure = Some((decoder.path().to_path_buf(), err.to_string()));
                        break;
                    }
                }
            }
        }

        if let Some((path, reason)) = failure {
            self.decoder = None;
            self.resampler = None;
            self.carry.clear();
            self.skip_failed(&path, &reason);
            return true;
        }

        if reached_eof {
            self.decoder = None;

            // Gapless: open the next track now and keep feeding the same ring,
            // so the last sample of this one is followed immediately by the
            // first sample of the next. `end_of_track` is deliberately left
            // clear — the stream has not ended, it has only changed track.
            if self.gapless && self.open_next_gaplessly() {
                return true;
            }

            self.shared.set_end_of_track(true);
        }

        self.update_priming();
        worked
    }

    /// Begin a crossfade if the current track is close enough to its end.
    ///
    /// Nothing happens without a known duration: a fade has to be scheduled
    /// against the end of the track, and a container that will not say how long
    /// it is cannot be scheduled against. Those tracks join gaplessly instead,
    /// which is the behaviour they had before crossfade existed.
    fn maybe_start_crossfade(&mut self) {
        if self.crossfade_secs <= 0.0 || self.fading.is_some() || self.decoder.is_none() {
            return;
        }

        let Some(duration) = self.decoder.as_ref().and_then(TrackDecoder::duration) else {
            return;
        };

        let rate = f64::from(self.shared.device_rate().max(1));
        let produced = self.decoded_frames as f64 / rate;
        let left = duration.as_secs_f64() - produced;

        if left > f64::from(self.crossfade_secs) {
            return;
        }

        let (Some(decoder), Some(resampler)) = (self.decoder.take(), self.resampler.take()) else {
            return;
        };

        // Park the outgoing track, then install the next one exactly the way a
        // gapless join does. Reusing that path means the seam boundary, and so
        // the moment the title, position and level correction change, is
        // decided in one place rather than two that can disagree.
        self.fading = Some(Fading::new(decoder, resampler));

        if self.open_next_gaplessly() {
            let total = (f64::from(self.crossfade_secs) * rate) as u64;
            self.fade = Fade {
                frame: 0,
                total: total.max(1),
            };
            return;
        }

        // Nothing to fade into: the queue is done. Put the outgoing track back
        // and let it finish the way it would have.
        if let Some(parked) = self.fading.take() {
            self.decoder = parked.decoder;
            self.resampler = Some(parked.resampler);
        }
    }

    /// Continue the buffered stream with the next queue entry.
    ///
    /// Returns whether a track was opened. `false` means the queue is finished
    /// (or the next file would not open), and the caller should fall back to
    /// ending normally.
    fn open_next_gaplessly(&mut self) -> bool {
        // Where in the buffered stream the new track begins. Everything already
        // pushed belongs to the track that just ended.
        let at_frame = self.shared.pushed_frames();

        loop {
            let Some(path) = self.queue.advance_after_playback().map(Path::to_path_buf) else {
                return false;
            };

            match TrackDecoder::open(&path) {
                Ok(mut decoder) => {
                    decoder.set_trim_silence(self.trim_silence);
                    let rate = decoder.sample_rate();
                    let channels = self.shared.device_channels();
                    let duration = decoder.duration();
                    let replay_gain_db = decoder.replay_gain().for_mode(self.replay_gain_mode);

                    match Resampler::new(rate, self.shared.device_rate(), channels) {
                        Ok(resampler) => {
                            self.resampler = Some(resampler);
                            self.decoder = Some(decoder);
                            self.decoded_frames = 0;

                            self.seam.push(Pending {
                                at_frame,
                                path,
                                index: self.queue.current_index().unwrap_or(0),
                                duration,
                                replay_gain_db,
                            });

                            return true;
                        }
                        Err(err) => {
                            // A file that cannot be resampled is reported and
                            // skipped, the same as one that cannot be decoded.
                            self.emit(Event::TrackFailed {
                                path,
                                reason: err.to_string(),
                            });
                        }
                    }
                }
                Err(err) => {
                    self.emit(Event::TrackFailed {
                        path,
                        reason: err.to_string(),
                    });
                }
            }
        }
    }

    /// Switch the reported track once playback actually crosses a boundary.
    ///
    /// Decoding runs ahead by up to a ring's worth of audio, so this cannot key
    /// off the decoder: the title, the elapsed time and the track's level
    /// correction all have to change when the *listener* reaches the seam, not
    /// when the worker does.
    fn advance_across_seams(&mut self) {
        while let Some(boundary) = self.seam.crossed(self.shared.position_frames()) {
            let consumed = boundary.at_frame;

            // Rebase both counters so position and duration are measured from
            // the start of the new track. The position uses an atomic subtract
            // because the callback is advancing it at the same time; the push
            // counter is only ever written here, so a plain store is fine.
            self.shared.rebase_position(consumed);
            self.shared
                .set_pushed_frames(self.shared.pushed_frames().saturating_sub(consumed));
            self.seam.rebase(consumed);

            self.shared.set_duration(boundary.duration);

            // The new track's level correction takes effect here, not when it
            // was opened - applying it earlier would retune the tail of the
            // previous track.
            self.track_gain_db = boundary.replay_gain_db;
            self.publish_params();

            self.emit(Event::TrackStarted {
                path: boundary.path,
                index: boundary.index,
                duration: boundary.duration,
            });
        }
    }

    /// Push whatever is left over from a previous attempt.
    ///
    /// Returns whether any progress was made.
    fn flush_carry(&mut self) -> bool {
        if self.carry.is_empty() {
            return false;
        }

        let Some(producer) = self.producer.as_mut() else {
            return false;
        };

        let (written, _) = producer.push_partial_slice(&self.carry);
        let pushed = written.len();
        if pushed == 0 {
            return false;
        }

        let channels = self.shared.device_channels().max(1);
        self.shared.add_pushed_frames((pushed / channels) as u64);
        self.carry.drain(..pushed);
        true
    }

    /// Let the callback start once there is a healthy cushion of audio.
    fn update_priming(&mut self) {
        if !self.shared.priming() {
            return;
        }

        // Roughly 300 ms, enough to survive a scheduling hiccup before the
        // first callback consumes any.
        let prime_target =
            (self.shared.device_rate() as usize * self.shared.device_channels().max(1)) * 3 / 10;

        let buffered = self
            .producer
            .as_ref()
            .map_or(0, |p| p.buffer().capacity() - p.slots());

        // A clip shorter than the prime target must not wait forever for audio
        // that will never arrive.
        if buffered >= prime_target || self.decoder.is_none() {
            self.shared.set_priming(false);
        }
    }

    /// Copy a converted block into the ring, keeping anything that did not fit.
    ///
    /// The remainder must never be discarded: a missing run of samples is a step
    /// discontinuity in the waveform, audible as a click or crackle no matter
    /// how low the volume is.
    fn push_block(
        producer: &mut rtrb::Producer<f32>,
        shared: &Shared,
        carry: &mut Vec<f32>,
        block: &[f32],
    ) {
        let (written, remainder) = producer.push_partial_slice(block);

        let channels = shared.device_channels().max(1);
        shared.add_pushed_frames((written.len() / channels) as u64);

        if !remainder.is_empty() {
            carry.extend_from_slice(remainder);
        }
    }

    /// The current track played to its end; move to the next one.
    fn advance_after_playback(&mut self) {
        let next = self.queue.advance_after_playback().map(Path::to_path_buf);

        match next {
            Some(path) => self.begin(path),
            None => self.finish_queue(),
        }
    }

    fn seek_to(&mut self, target: Duration) {
        let Some(decoder) = self.decoder.as_mut() else {
            return;
        };

        if let Err(err) = decoder.seek(target) {
            tracing::warn!("seek failed: {err:#}");
            return;
        }

        if let Some(resampler) = self.resampler.as_mut() {
            resampler.reset();
        }

        // Raised before the flush: `discard_buffered` spins waiting for the
        // callback to acknowledge, and the callback runs during that spin. Left
        // until afterwards it would see an empty ring and call it an underrun.
        self.shared.set_priming(true);
        self.carry.clear();
        self.discard_buffered();

        // Re-anchor both counters to the new position so progress and
        // end-of-track detection stay consistent.
        let frames = (target.as_secs_f64() * f64::from(self.shared.device_rate())) as u64;
        self.shared.set_position_frames(frames);
        self.shared.set_pushed_frames(frames);
        self.shared.set_end_of_track(false);
    }

    /// Drop everything queued for playback and wait for the callback to agree.
    fn discard_buffered(&mut self) {
        let sequence = self.shared.request_flush();

        let deadline = Instant::now() + FLUSH_TIMEOUT;
        while !self.shared.flush_acknowledged(sequence) {
            if Instant::now() >= deadline {
                tracing::warn!("audio callback did not acknowledge a flush");
                break;
            }
            std::thread::yield_now();
        }
    }

    fn stop(&mut self) {
        self.decoder = None;
        self.resampler = None;
        self.fading = None;
        self.decoded_frames = 0;
        self.carry.clear();
        self.shared.set_priming(true);
        self.discard_buffered();
        self.shared.set_status(Status::Stopped);
        self.shared.reset_for_new_track(None);
    }

    fn finish_queue(&mut self) {
        self.stop();
        self.emit(Event::QueueFinished);
    }

    fn reopen_device(&mut self, name: Option<String>, buffer_frames: Option<u32>) {
        // Dropping the stream stops the callback, so nothing is reading the old
        // ring by the time it is replaced.
        self.stream = None;
        self.producer = None;

        match device::open(name.as_deref(), buffer_frames) {
            Ok(output) => {
                self.output = Some(output);
                if let Err(err) = self.start_stream() {
                    tracing::error!("could not reopen the audio output: {err:#}");
                    return;
                }

                // The device rate may have changed, so the resampler built for
                // the old one is no longer valid.
                if let Some(decoder) = self.decoder.as_ref() {
                    let from = decoder.sample_rate();
                    let to = self.shared.device_rate();
                    let channels = self.shared.device_channels();

                    match Resampler::new(from, to, channels) {
                        Ok(resampler) => self.resampler = Some(resampler),
                        Err(err) => {
                            tracing::error!("could not rebuild the resampler: {err:#}");
                            self.stop();
                        }
                    }
                }
            }
            Err(err) => {
                tracing::error!("could not open the audio output: {err:#}");
                self.shared.set_status(Status::Stopped);
            }
        }
    }

    fn emit(&self, event: Event) {
        // A full or closed channel means the UI is gone; nothing to do about it.
        let _ = self.events.send(event);
    }
}

enum ControlFlow {
    Continue,
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression this guards is a real one that shipped: `push_partial_slice`
    /// returns `(written, remainder)`, and the remainder was being thrown away
    /// whenever the ring had less room than the block. On a 96 kHz 8-channel
    /// device an upsampled block is ~18k samples while the old guard only
    /// reserved 8192, so thousands of samples were discarded continuously —
    /// heard as crackling at any volume, because a missing run of samples is a
    /// step discontinuity rather than an amplitude problem.
    #[test]
    fn a_block_too_large_for_the_ring_is_carried_not_dropped() {
        let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(100);
        let shared = Shared::new();
        shared.set_device(48_000, 2);
        let mut carry = Vec::new();

        // Distinct values so any gap or reordering is detectable.
        let block: Vec<f32> = (0..250).map(|i| i as f32).collect();

        Worker::push_block(&mut producer, &shared, &mut carry, &block);

        assert_eq!(producer.slots(), 0, "the ring should be completely full");
        assert_eq!(carry.len(), 150, "everything that did not fit must be kept");

        // Drain and re-push until everything has gone through. The ring holds
        // less than the remainder, so this takes several rounds - which is
        // exactly the situation the old code silently discarded.
        let mut received = Vec::new();
        let mut rounds = 0;

        loop {
            while let Ok(sample) = consumer.pop() {
                received.push(sample);
            }

            if carry.is_empty() {
                break;
            }

            let pending = std::mem::take(&mut carry);
            Worker::push_block(&mut producer, &shared, &mut carry, &pending);

            rounds += 1;
            assert!(rounds < 10, "carry-over is not draining");
        }

        assert_eq!(
            received, block,
            "every sample arrives, in order, exactly once"
        );
    }

    /// Frame accounting must follow what actually reached the ring, or
    /// end-of-track detection fires while audio is still buffered.
    #[test]
    fn only_accepted_samples_are_counted_as_pushed() {
        let (mut producer, _consumer) = rtrb::RingBuffer::<f32>::new(64);
        let shared = Shared::new();
        shared.set_device(48_000, 2);
        let mut carry = Vec::new();

        let block = vec![0.25f32; 200];
        Worker::push_block(&mut producer, &shared, &mut carry, &block);

        // 64 samples of a 2-channel stream is 32 frames.
        assert_eq!(shared.pushed_frames(), 32);
        assert_eq!(carry.len(), 136);
    }

    /// An outgoing stream with nothing left to decode.
    fn drained(ready: &[f32]) -> Fading {
        Fading {
            decoder: None,
            resampler: Resampler::new(48_000, 48_000, 2).expect("passthrough resampler"),
            ready: ready.iter().copied().collect(),
        }
    }

    #[test]
    fn a_drained_outgoing_track_pads_with_silence() {
        // The failure this guards: leaving the caller's buffer untouched past
        // the end of the outgoing track would mix whatever was in it last time
        // back into the fade, as a burst of stale audio.
        let mut fading = drained(&[0.5, -0.5]);
        let mut out = vec![9.0; 6];

        fading.take(&mut out);

        assert_eq!(out, vec![0.5, -0.5, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn an_outgoing_track_is_handed_out_in_order_across_calls() {
        let mut fading = drained(&[1.0, 2.0, 3.0, 4.0]);

        let mut first = vec![0.0; 2];
        fading.take(&mut first);
        let mut second = vec![0.0; 2];
        fading.take(&mut second);

        assert_eq!(first, vec![1.0, 2.0]);
        assert_eq!(second, vec![3.0, 4.0], "the fade must not repeat samples");
    }

    #[test]
    fn a_fully_drained_outgoing_track_is_pure_silence() {
        let mut fading = drained(&[]);
        let mut out = vec![7.0; 4];

        fading.take(&mut out);

        assert!(out.iter().all(|s| *s == 0.0), "{out:?}");
    }

    #[test]
    fn a_fade_starts_at_the_beginning_of_its_length() {
        let fade = Fade::default();
        assert_eq!(fade.frame, 0);
        assert_eq!(fade.total, 0, "a zero total means no fade is running");
    }
}

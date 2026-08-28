//! A single decoded track: open, pull planar `f32` frames, seek.
//!
//! This wraps `symphonia` 0.6, whose audio API differs substantially from 0.5:
//! packets arrive as `GenericAudioBufferRef` (a sample-format enum) rather than
//! a concrete buffer type, and `next_packet` reports end-of-stream as `Ok(None)`
//! instead of as an error.
//!
//! Output is **planar** (`Vec<Vec<f32>>`, one vec per channel) because that is
//! what the resampler consumes; interleaving happens once, on the way out.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::{MetadataOptions, StandardTag};
use symphonia::core::units::Time;

use crate::error::{AudioError, Result};

/// Level correction written into a file by a ReplayGain scanner.
///
/// Both are in decibels relative to the reference loudness, and both are
/// usually absent — most of a downloaded collection has never been scanned.
/// Absent means "leave the level alone", which is the right default: guessing
/// at a correction is worse than applying none.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ReplayGain {
    /// Correction for this track played on its own.
    pub track_db: Option<f32>,
    /// Correction that keeps an album's relative dynamics intact.
    pub album_db: Option<f32>,
}

impl ReplayGain {
    pub fn is_empty(&self) -> bool {
        self.track_db.is_none() && self.album_db.is_none()
    }

    /// The value for a given mode, falling back to the other when the
    /// preferred one is missing.
    ///
    /// A partially tagged file is common — album gain without track gain, or
    /// the reverse — and using the one that is present beats using neither.
    pub fn for_mode(&self, mode: mp_core::config::ReplayGainMode) -> Option<f32> {
        use mp_core::config::ReplayGainMode;
        match mode {
            ReplayGainMode::Off => None,
            ReplayGainMode::Track => self.track_db.or(self.album_db),
            ReplayGainMode::Album => self.album_db.or(self.track_db),
        }
    }
}

/// ReplayGain values are text, written as `-7.25 dB`.
fn parse_gain_db(value: &str) -> Option<f32> {
    let cleaned = value
        .trim()
        .trim_end_matches(|c: char| c.is_ascii_alphabetic() || c.is_whitespace())
        .trim();

    let parsed: f32 = cleaned.parse().ok()?;

    // A correction beyond this is not a measurement, it is a corrupt tag, and
    // applying it would either deafen the listener or silence the track.
    (parsed.is_finite() && parsed.abs() <= 60.0).then_some(parsed)
}

/// A block of decoded audio, planar and at the file's own sample rate.
pub struct DecodedChunk {
    /// One vec per channel, all the same length.
    pub planes: Vec<Vec<f32>>,
    /// Frames in this chunk (i.e. the length of each plane).
    pub frames: usize,
}

/// An open, decoding audio file.
pub struct TrackDecoder {
    path: PathBuf,
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,

    sample_rate: u32,
    channels: usize,
    duration: Option<Duration>,
    replay_gain: ReplayGain,

    /// Reused across calls so steady-state decoding does not allocate.
    scratch: Vec<Vec<f32>>,
}

impl TrackDecoder {
    /// Open a file and prepare its first audio track for decoding.
    pub fn open(path: &Path) -> Result<Self> {
        // Reject by extension first, so the error names the format instead of
        // being a generic "unsupported container" from deep inside symphonia.
        if let crate::format::Support::Unsupported { reason } = crate::format::classify(path) {
            return Err(AudioError::Unsupported {
                path: path.to_owned(),
                reason: reason.to_owned(),
            });
        }

        let file = File::open(path).map_err(|source| AudioError::Io {
            path: path.to_owned(),
            source,
        })?;

        let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());

        // The extension is a hint only; symphonia still sniffs the content, so a
        // mislabelled file opens correctly as long as we can decode what it
        // really is.
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        // The scrubber seeks constantly and formats like MP3 carry no native
        // index, so pay the cost of building one up front.
        let mut fmt_opts = FormatOptions::default();
        fmt_opts.prebuild_seek_index = true;

        let reader = symphonia::default::get_probe()
            .probe(&hint, mss, fmt_opts, MetadataOptions::default())
            .map_err(|source| AudioError::Decode {
                path: path.to_owned(),
                source,
            })?;

        let track = reader
            .first_track_known_codec(TrackType::Audio)
            .ok_or_else(|| AudioError::NoAudioTrack {
                path: path.to_owned(),
            })?;

        let track_id = track.id;

        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| AudioError::NoAudioTrack {
                path: path.to_owned(),
            })?;

        // Duration comes from the container and can be absent (a stream, or a
        // truncated file). The UI renders that as an unknown length rather than
        // guessing at one.
        let duration = track
            .time_base
            .zip(track.duration)
            .and_then(|(tb, dur)| tb.calc_duration(dur))
            .map(|time| Duration::from_secs_f64(time.as_secs_f64().max(0.0)));

        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|source| no_decoder_error(path, source))?;

        // Sample rate and channel count are sometimes only known once the first
        // packet has been decoded; seed them from the container and correct
        // them as chunks arrive.
        let sample_rate = params.sample_rate.unwrap_or(44_100);
        let channels = params.channels.as_ref().map_or(2, |c| c.count());

        let mut decoded = Self {
            path: path.to_owned(),
            reader,
            decoder,
            track_id,
            sample_rate,
            channels,
            duration,
            replay_gain: ReplayGain::default(),
            scratch: Vec::new(),
        };

        // Read while the file is open anyway. Doing it here rather than from
        // the library index means level correction also works for a file played
        // from outside the library.
        decoded.replay_gain = decoded.read_replay_gain();

        Ok(decoded)
    }

    /// Pull ReplayGain values out of whatever tag block the container carries.
    fn read_replay_gain(&mut self) -> ReplayGain {
        let mut gain = ReplayGain::default();

        let mut metadata = self.reader.metadata();
        let Some(revision) = metadata.skip_to_latest() else {
            return gain;
        };

        for tag in &revision.media.tags {
            match tag.std.as_ref() {
                Some(StandardTag::ReplayGainTrackGain(value)) => {
                    gain.track_db = parse_gain_db(value.as_str());
                }
                Some(StandardTag::ReplayGainAlbumGain(value)) => {
                    gain.album_db = parse_gain_db(value.as_str());
                }
                _ => {}
            }
        }

        gain
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn duration(&self) -> Option<Duration> {
        self.duration
    }

    /// Level-correction values from the file's own tags.
    pub fn replay_gain(&self) -> ReplayGain {
        self.replay_gain
    }

    /// Decode the next block of audio.
    ///
    /// Returns `Ok(None)` at end of stream. Recoverable per-packet corruption is
    /// skipped rather than propagated: a damaged frame in the middle of an MP3
    /// should cost a few milliseconds of audio, not the rest of the track.
    pub fn next_chunk(&mut self) -> Result<Option<DecodedChunk>> {
        loop {
            let packet = match self.reader.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => return Ok(None),
                Err(SymphoniaError::IoError(err))
                    if err.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    // Some encoders leave a truncated final packet; treat it as
                    // a normal end rather than an error.
                    return Ok(None);
                }
                Err(source) => {
                    return Err(AudioError::Decode {
                        path: self.path.clone(),
                        source,
                    });
                }
            };

            // Containers can interleave several tracks; ignore the others.
            if packet.track_id != self.track_id {
                continue;
            }

            match self.decoder.decode(&packet) {
                Ok(buffer) => {
                    if buffer.frames() == 0 {
                        continue;
                    }
                    let spec = buffer.spec();
                    let rate = spec.rate();
                    let channels = spec.channels().count();
                    let frames = buffer.frames();

                    // `copy_to_vecs_planar` resizes the destination as needed, so
                    // reusing `scratch` keeps steady-state decoding allocation-free.
                    buffer.copy_to_vecs_planar::<f32>(&mut self.scratch);

                    self.sample_rate = rate;
                    self.channels = channels;

                    return Ok(Some(DecodedChunk {
                        planes: std::mem::take(&mut self.scratch),
                        frames,
                    }));
                }
                Err(SymphoniaError::DecodeError(err)) => {
                    // Corrupt packet: note it and carry on with the next one.
                    tracing::debug!("skipping bad packet in {}: {err}", self.path.display());
                    continue;
                }
                Err(SymphoniaError::ResetRequired) => {
                    // The stream changed parameters mid-file (rare, but legal in
                    // Ogg). Rebuild the decoder and keep going.
                    self.reset_decoder()?;
                    continue;
                }
                Err(source) => {
                    return Err(AudioError::Decode {
                        path: self.path.clone(),
                        source,
                    });
                }
            }
        }
    }

    /// Hand a chunk's buffers back for reuse on the next decode.
    ///
    /// Optional: dropping a chunk is perfectly safe. This only avoids
    /// reallocating one vec per channel per packet.
    pub fn recycle(&mut self, mut chunk: DecodedChunk) {
        if self.scratch.is_empty() {
            for plane in &mut chunk.planes {
                plane.clear();
            }
            self.scratch = chunk.planes;
        }
    }

    /// Seek to `target`, as accurately as the container allows.
    pub fn seek(&mut self, target: Duration) -> Result<()> {
        let Some(time) = Time::try_from_secs_f64(target.as_secs_f64()) else {
            // Only reachable for a non-finite or absurd target; ignore it
            // rather than failing the seek.
            return Ok(());
        };

        let result = self.reader.seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time,
                track_id: Some(self.track_id),
            },
        );

        match result {
            Ok(_) => {
                // Mandatory after a seek: the decoder holds state from the old
                // position and would otherwise emit a burst of garbage.
                self.decoder.reset();
                Ok(())
            }
            // A format with no seek support (or a seek past the end) should not
            // kill playback; stay where we are.
            Err(SymphoniaError::Unsupported(_) | SymphoniaError::SeekError(_)) => {
                tracing::debug!("{} does not support seeking", self.path.display());
                Ok(())
            }
            Err(source) => Err(AudioError::Decode {
                path: self.path.clone(),
                source,
            }),
        }
    }

    /// Rebuild the decoder after the stream signalled a parameter change.
    fn reset_decoder(&mut self) -> Result<()> {
        let track = self
            .reader
            .tracks()
            .iter()
            .find(|t| t.id == self.track_id)
            .ok_or_else(|| AudioError::NoAudioTrack {
                path: self.path.clone(),
            })?;

        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| AudioError::NoAudioTrack {
                path: self.path.clone(),
            })?;

        self.decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|source| no_decoder_error(&self.path, source))?;

        Ok(())
    }
}

/// Turn "unsupported audio codec" into something a person can act on.
///
/// A file whose extension lies about its contents decodes right up until the
/// codec is one this build lacks. Symphonia can only report that the codec is
/// unknown; reading the header lets us name the format the file actually is.
fn no_decoder_error(path: &Path, source: SymphoniaError) -> AudioError {
    if let Some(actual) = crate::format::sniff(path) {
        let declared = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());

        let reason = match declared {
            Some(ext) => format!(
                "this file is named .{ext} but is actually {actual}, which this build cannot decode"
            ),
            None => format!("this file is {actual}, which this build cannot decode"),
        };

        return AudioError::Unsupported {
            path: path.to_owned(),
            reason,
        };
    }

    AudioError::Decode {
        path: path.to_owned(),
        source,
    }
}

//! Decode an audio file (WAV, MP3, M4A/AAC, FLAC, ALAC, Ogg/Vorbis, the audio
//! track of MP4/MOV) to 16 kHz mono `f32`, the input every speech model takes.

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};
use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

pub const SAMPLE_RATE: u32 = 16_000;

/// Decoded, down-mixed and resampled audio.
pub struct Audio {
    pub samples: Vec<f32>,
    /// The file's own sample rate, before resampling.
    pub source_rate: u32,
}

impl Audio {
    pub fn duration_s(&self) -> f64 {
        self.samples.len() as f64 / SAMPLE_RATE as f64
    }

    /// Samples between two times, clamped to the audio.
    pub fn slice(&self, start_s: f64, end_s: f64) -> &[f32] {
        let at = |t: f64| ((t.max(0.0) * SAMPLE_RATE as f64) as usize).min(self.samples.len());
        let (a, b) = (at(start_s), at(end_s));
        &self.samples[a..b.max(a)]
    }
}

/// Read `path` and return 16 kHz mono samples.
pub fn decode_file(path: &Path) -> Result<Audio> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_owned);
    decode(MediaSourceStream::new(Box::new(file), Default::default()), ext.as_deref())
        .with_context(|| format!("reading {}", path.display()))
}

/// Decode audio held in memory (an upload). `ext` is a hint such as "m4a".
/// Nothing is written to disk.
pub fn decode_bytes(bytes: Vec<u8>, ext: Option<&str>) -> Result<Audio> {
    decode(MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), Default::default()), ext)
}

fn decode(mss: MediaSourceStream, ext: Option<&str>) -> Result<Audio> {
    let mut hint = Hint::new();
    if let Some(ext) = ext {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .context("this isn't an audio format we can read")?;
    let track = format
        .default_track(TrackType::Audio)
        .context("the file has no audio track")?;
    let track_id = track.id;
    let codec_params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .context("the audio track has no codec parameters")?
        .clone();
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
        .context("no decoder for this audio codec")?;

    let mut mono: Vec<f32> = Vec::new();
    let mut rate = codec_params.sample_rate.unwrap_or(0);
    let mut interleaved: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => return Err(e).context("reading the audio file"),
        };
        if packet.track_id != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // A damaged packet: skip it, like ffmpeg does.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(e).context("decoding audio"),
        };
        append_mono(&decoded, &mut interleaved, &mut mono);
        if rate == 0 {
            rate = decoded.spec().rate();
        }
    }
    if mono.is_empty() {
        bail!("the file contains no audio");
    }
    if rate == 0 {
        bail!("the file doesn't say its sample rate");
    }
    let samples = if rate == SAMPLE_RATE { mono } else { resample(&mono, rate, SAMPLE_RATE)? };
    Ok(Audio { samples, source_rate: rate })
}

fn append_mono(buf: &GenericAudioBufferRef<'_>, scratch: &mut Vec<f32>, out: &mut Vec<f32>) {
    let channels = buf.spec().channels().count().max(1);
    scratch.clear();
    buf.copy_to_vec_interleaved(scratch);
    out.reserve(scratch.len() / channels);
    for frame in scratch.chunks_exact(channels) {
        out.push(frame.iter().sum::<f32>() / channels as f32);
    }
}

/// Resample mono audio with an FFT resampler (fixed ratio, high quality).
pub fn resample(input: &[f32], from: u32, to: u32) -> Result<Vec<f32>> {
    let mut resampler = Fft::<f32>::new(from as usize, to as usize, 1024, 1, FixedSync::Input)
        .context("setting up the resampler")?;
    let adapter = InterleavedSlice::new(input, 1, input.len()).context("audio buffer")?;
    let out = resampler.process_all(&adapter, input.len(), None).context("resampling")?;
    Ok(out.take_data())
}

/// Resamples a stream piece by piece, for live capture: push what arrives,
/// take what's ready. The same FFT resampler as [`resample`]. The
/// resampler's start-up delay is dropped, so output sample `n` is the same
/// moment as input sample `n × ratio`: times in a recording stay true.
pub struct StreamResampler {
    /// `None` when the rates match: samples pass through.
    fft: Option<Fft<f32>>,
    pending: Vec<f32>,
    scratch: Vec<f32>,
    /// Output frames still to drop (the delay).
    skip: usize,
    input_total: u64,
    output_total: u64,
}

impl StreamResampler {
    pub fn new(from: u32, to: u32) -> Result<Self> {
        let fft = if from == to {
            None
        } else {
            Some(Fft::<f32>::new(from as usize, to as usize, 1024, 1, FixedSync::Input).context("setting up the resampler")?)
        };
        let scratch = vec![0.0; fft.as_ref().map(|f| f.output_frames_max()).unwrap_or(0)];
        let skip = fft.as_ref().map(|f| f.output_delay()).unwrap_or(0);
        Ok(StreamResampler { fft, pending: Vec::new(), scratch, skip, input_total: 0, output_total: 0 })
    }

    /// Add mono samples; whatever is ready is appended to `out`.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<()> {
        self.input_total += input.len() as u64;
        if self.fft.is_none() {
            out.extend_from_slice(input);
            self.output_total += input.len() as u64;
            return Ok(());
        }
        self.pending.extend_from_slice(input);
        let mut used_total = 0;
        loop {
            let need = self.fft.as_ref().unwrap().input_frames_next();
            if self.pending.len() - used_total < need {
                break;
            }
            let chunk = self.pending[used_total..used_total + need].to_vec();
            let (used, _) = self.run(&chunk, None, out)?;
            used_total += used;
        }
        self.pending.drain(..used_total);
        Ok(())
    }

    /// The last samples (padded with silence) and the delay still inside
    /// the resampler; call once at the end. Total output then matches the
    /// input's length at the new rate.
    pub fn flush(&mut self, out: &mut Vec<f32>) -> Result<()> {
        let Some(fft) = self.fft.as_ref() else { return Ok(()) };
        let expected = (self.input_total as f64 * fft.resample_ratio()).round() as u64;
        let mut tail = Vec::new();
        let mut rest = std::mem::take(&mut self.pending);
        // The real samples first, then silence until the delay has come out;
        // a few chunks at most.
        for _ in 0..64 {
            if rest.is_empty() && self.output_total + tail.len() as u64 >= expected {
                break;
            }
            let need = self.fft.as_ref().unwrap().input_frames_next();
            let valid = rest.len().min(need);
            let mut chunk: Vec<f32> = rest.drain(..valid).collect();
            chunk.resize(need, 0.0);
            self.run(&chunk, Some(valid), &mut tail)?;
        }
        let room = expected.saturating_sub(self.output_total) as usize;
        tail.truncate(room);
        self.output_total += tail.len() as u64;
        out.extend_from_slice(&tail);
        Ok(())
    }

    /// One chunk through the resampler; drops the delay, counts output.
    fn run(&mut self, chunk: &[f32], partial: Option<usize>, out: &mut Vec<f32>) -> Result<(usize, usize)> {
        let fft = self.fft.as_mut().unwrap();
        let src = InterleavedSlice::new(chunk, 1, chunk.len()).context("audio buffer")?;
        let frames = self.scratch.len();
        let mut dst = InterleavedSlice::new_mut(&mut self.scratch[..], 1, frames).context("audio buffer")?;
        let indexing = partial.map(|n| Indexing { input_offset: 0, output_offset: 0, partial_len: Some(n), active_channels_mask: None });
        let (used, made) = fft.process_into_buffer(&src, &mut dst, indexing.as_ref()).context("resampling")?;
        let drop = self.skip.min(made);
        self.skip -= drop;
        let fresh = &self.scratch[drop..made];
        // In flush, the caller counts and trims; in push, count here.
        if partial.is_none() {
            self.output_total += fresh.len() as u64;
        }
        out.extend_from_slice(fresh);
        Ok((used, made))
    }
}

/// Mono samples as a 16-bit PCM WAV file, for sending a stretch of audio.
pub fn wav_i16(samples: &[f32], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        wav.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
    }
    wav
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_keeps_duration_and_pitch() {
        // One second of 440 Hz at 48 kHz.
        let input: Vec<f32> =
            (0..48_000).map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin()).collect();
        let out = resample(&input, 48_000, 16_000).unwrap();
        assert!((out.len() as i64 - 16_000).abs() <= 16, "got {} samples", out.len());
        // Count zero crossings: 440 Hz → ~880 per second.
        let crossings = out.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        assert!((860..=900).contains(&crossings), "{crossings} crossings");
    }

    #[test]
    fn streaming_matches_whole_buffer_resampling() {
        let input: Vec<f32> =
            (0..48_000).map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin()).collect();
        // Odd-sized pieces, as a capture callback delivers them.
        let mut r = StreamResampler::new(48_000, 16_000).unwrap();
        let mut out = Vec::new();
        for piece in input.chunks(997) {
            r.push(piece, &mut out).unwrap();
        }
        r.flush(&mut out).unwrap();
        assert_eq!(out.len(), 16_000, "exactly the input's length at the new rate");
        // The delay is gone: the sine starts at the first sample, like the input.
        let whole = resample(&input, 48_000, 16_000).unwrap();
        let lag = (0..40).min_by(|&a, &b| {
            let err = |l: usize| out[l..4000 + l].iter().zip(&whole[..4000]).map(|(x, y)| (x - y).abs()).sum::<f32>();
            err(a).total_cmp(&err(b))
        });
        assert!(lag.unwrap() <= 1, "streaming is {lag:?} samples late");
        let crossings = out.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        let per_second = crossings as f64 * 16_000.0 / out.len() as f64;
        assert!((860.0..=900.0).contains(&per_second), "{per_second} crossings per second");
        // Same rate: samples pass through untouched.
        let mut same = StreamResampler::new(16_000, 16_000).unwrap();
        let mut copy = Vec::new();
        same.push(&input[..100], &mut copy).unwrap();
        assert_eq!(copy, input[..100]);
    }

    #[test]
    fn very_short_streams_keep_their_length() {
        let mut r = StreamResampler::new(44_100, 16_000).unwrap();
        let mut out = Vec::new();
        r.push(&[0.25; 300], &mut out).unwrap();
        r.flush(&mut out).unwrap();
        assert_eq!(out.len(), (300.0f64 * 16_000.0 / 44_100.0).round() as usize);
    }

    #[test]
    fn a_window_round_trips_as_wav() {
        let samples: Vec<f32> = (0..1600).map(|i| (i as f32 / 1600.0) * 2.0 - 1.0).collect();
        let back = decode_bytes(wav_i16(&samples, 16_000), Some("wav")).unwrap();
        assert_eq!(back.samples.len(), samples.len());
        assert!(back.samples.iter().zip(&samples).all(|(a, b)| (a - b).abs() < 1e-3));
    }

    #[test]
    fn decodes_a_wav_from_memory() {
        // 0.5 s of 16-bit mono silence with a 440 Hz burst, as a WAV file in memory.
        let n = 8_000u32;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + n * 2).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&16_000u32.to_le_bytes());
        wav.extend_from_slice(&32_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(n * 2).to_le_bytes());
        for i in 0..n {
            let s = ((i as f32 * 440.0 * std::f32::consts::TAU / 16_000.0).sin() * 8_000.0) as i16;
            wav.extend_from_slice(&s.to_le_bytes());
        }
        let audio = decode_bytes(wav, Some("wav")).unwrap();
        assert_eq!(audio.samples.len(), n as usize);
        assert!((audio.duration_s() - 0.5).abs() < 1e-9);
        assert!(audio.samples.iter().any(|&s| s.abs() > 0.2));
        assert!(decode_bytes(b"not audio".to_vec(), None).is_err());
    }

    #[test]
    fn slices_are_clamped() {
        let audio = Audio { samples: vec![0.0; 16_000], source_rate: 16_000 };
        assert_eq!(audio.slice(0.5, 0.75).len(), 4_000);
        assert_eq!(audio.slice(0.9, 5.0).len(), 1_600);
        assert_eq!(audio.slice(-1.0, 0.1).len(), 1_600);
        assert!(audio.slice(2.0, 3.0).is_empty());
        assert!(audio.slice(0.5, 0.4).is_empty());
    }
}

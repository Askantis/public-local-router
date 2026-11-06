//! Voiceprints: one embedding per speaker, for apps that recognise people
//! they were told to remember. A voiceprint is the mean of the speaker's
//! WeSpeaker embeddings (each L2-normalised first), L2-normalised again, so
//! two voiceprints compare by their dot product (cosine similarity).
//!
//! Voiceprints are biometric data. The engine computes them only when asked
//! (PROTOCOL.md `lr_voiceprints`) and keeps none: storing them, with the
//! person's consent, is up to the app.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use speakrs::ModelBundle;
use speakrs::inference::EmbeddingModel;
use speakrs::pipeline::DiarizationResult;

use crate::{SAMPLE_RATE, Turn};

/// The embedding model voiceprints come from. Voiceprints of different
/// models don't compare.
pub const VOICEPRINT_MODEL: &str = "pyannote/wespeaker-voxceleb-resnet34";
/// Values in a voiceprint.
pub const VOICEPRINT_DIM: usize = 256;

/// The embedding model's window: 10 s of audio.
const WINDOW_SAMPLES: usize = 10 * SAMPLE_RATE as usize;
/// Mask frames over one window (speakrs' `MASK_FRAMES`).
const MASK_FRAMES: usize = 589;
/// Less speech in a window than this makes an unreliable embedding; such
/// windows count only when a speaker has nothing better.
const MIN_WINDOW_SPEECH_S: f64 = 1.0;
/// Windows embedded per speaker at most (the ones with the most speech):
/// about 5 minutes of speech make a stable voiceprint, and an hour-long
/// speaker doesn't take minutes.
const MAX_WINDOWS: usize = 30;

/// A speaker's voice.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Voiceprint {
    /// [`VOICEPRINT_MODEL`].
    pub model: String,
    /// [`VOICEPRINT_DIM`] values, L2-normalised.
    pub vector: Vec<f32>,
    /// Seconds of the speaker's speech it was computed from: more is more reliable.
    pub speech_s: f64,
}

impl Voiceprint {
    /// Cosine similarity, from −1 to 1; `None` for different models or sizes.
    pub fn similarity(&self, other: &Voiceprint) -> Option<f32> {
        (self.model == other.model && self.vector.len() == other.vector.len()).then(|| cosine(&self.vector, &other.vector))
    }
}

/// Cosine similarity of two vectors (0 when either is all zeros).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        dot += (*x as f64) * (*y as f64);
        na += (*x as f64) * (*x as f64);
        nb += (*y as f64) * (*y as f64);
    }
    if na == 0.0 || nb == 0.0 { 0.0 } else { (dot / (na.sqrt() * nb.sqrt())) as f32 }
}

/// Sums weighted, normalised embeddings into a voiceprint.
#[derive(Default)]
struct Mean {
    sum: Vec<f64>,
    weight: f64,
}

impl Mean {
    fn add(&mut self, e: impl IntoIterator<Item = f32>, weight: f64) {
        let e: Vec<f32> = e.into_iter().collect();
        if weight <= 0.0 || e.iter().any(|x| !x.is_finite()) {
            return;
        }
        let norm = e.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
        if norm < 1e-9 {
            return;
        }
        if self.sum.is_empty() {
            self.sum = vec![0.0; e.len()];
        }
        if self.sum.len() != e.len() {
            return;
        }
        for (s, x) in self.sum.iter_mut().zip(&e) {
            *s += weight * (*x as f64) / norm;
        }
        self.weight += weight;
    }

    fn finish(&self, speech_s: f64) -> Option<Voiceprint> {
        let norm = self.sum.iter().map(|x| x * x).sum::<f64>().sqrt();
        (self.weight > 0.0 && norm > 1e-9).then(|| Voiceprint {
            model: VOICEPRINT_MODEL.into(),
            vector: self.sum.iter().map(|x| (x / norm) as f32).collect(),
            speech_s: (speech_s * 100.0).round() / 100.0,
        })
    }
}

/// Seconds each speaker talks in `turns`.
fn speech_seconds(turns: &[Turn]) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    for t in turns {
        *out.entry(t.speaker.clone()).or_insert(0.0) += (t.end - t.start).max(0.0);
    }
    out
}

/// Voiceprints of a diarization's speakers ("SPEAKER_00", … as in its
/// segments) from the embeddings it computed anyway: every chunk where the
/// speaker talks, weighted by how much they talk in it. Free: no model runs.
pub(crate) fn from_diarization(result: &DiarizationResult, exclusive: &[Turn]) -> BTreeMap<String, Voiceprint> {
    let emb = &result.embeddings.0;
    let clusters = &result.hard_clusters.0;
    let seg = &result.segmentations.0;
    let (chunks, local) = (clusters.nrows(), clusters.ncols());
    if emb.shape().len() != 3 || emb.shape()[0] < chunks || emb.shape()[1] < local || seg.shape()[0] < chunks || seg.shape()[2] < local {
        return BTreeMap::new();
    }
    let (dim, frames) = (emb.shape()[2], seg.shape()[1]);
    let mut means: BTreeMap<i32, Mean> = BTreeMap::new();
    for c in 0..chunks {
        for s in 0..local {
            let k = clusters[[c, s]];
            if k < 0 {
                continue;
            }
            let activity: f64 = (0..frames).map(|f| seg[[c, f, s]] as f64).sum();
            means.entry(k).or_default().add((0..dim).map(|d| emb[[c, s, d]]), activity);
        }
    }
    let speech = speech_seconds(exclusive);
    means
        .into_iter()
        .filter_map(|(k, m)| {
            let label = format!("SPEAKER_{k:02}");
            let secs = speech.get(&label).copied().unwrap_or(0.0);
            m.finish(secs).map(|v| (label, v))
        })
        .collect()
}

/// One 10 s window of a speaker: where it starts and which of its mask
/// frames the speaker talks in.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Window {
    pub start: usize,
    pub mask: Vec<f32>,
    pub speech_s: f64,
}

/// The windows to embed for one speaker's `turns` (sorted, not overlapping
/// other speakers'): each starts where the speaker starts talking and covers
/// 10 s, masked to the speaker's speech; the next starts at the next turn
/// after it. The [`MAX_WINDOWS`] with the most speech, at least
/// [`MIN_WINDOW_SPEECH_S`] each unless there's nothing better.
pub(crate) fn windows_for(turns: &[(f64, f64)], total_samples: usize) -> Vec<Window> {
    let sr = SAMPLE_RATE as f64;
    let win_s = WINDOW_SAMPLES as f64 / sr;
    let frame_s = win_s / MASK_FRAMES as f64;
    let mut out: Vec<Window> = Vec::new();
    let mut next_free = f64::NEG_INFINITY;
    for &(ts, te) in turns {
        let mut start = ts.max(next_free);
        while start < te {
            let ws = start;
            let we = ws + win_s;
            let mut mask = vec![0.0f32; MASK_FRAMES];
            let mut speech = 0.0;
            for &(a, b) in turns.iter().filter(|&&(a, b)| b > ws && a < we) {
                let (a, b) = (a.max(ws), b.min(we));
                speech += b - a;
                let from = ((a - ws) / frame_s).floor() as usize;
                let to = (((b - ws) / frame_s).ceil() as usize).min(MASK_FRAMES);
                mask[from.min(MASK_FRAMES)..to].fill(1.0);
            }
            let start_sample = ((ws * sr) as usize).min(total_samples);
            // Past the end of the audio there's nothing to hear.
            let audible = total_samples.saturating_sub(start_sample) as f64 / sr;
            if audible < win_s {
                let last = ((audible / frame_s).floor() as usize).min(MASK_FRAMES);
                mask[last..].fill(0.0);
            }
            if speech > 0.0 && mask.iter().any(|&m| m > 0.0) {
                out.push(Window { start: start_sample, mask, speech_s: speech.min(audible.max(0.0)) });
            }
            next_free = we;
            start = we;
        }
    }
    let good = out.iter().filter(|w| w.speech_s >= MIN_WINDOW_SPEECH_S).count();
    if good > 0 {
        out.retain(|w| w.speech_s >= MIN_WINDOW_SPEECH_S);
    }
    if out.len() > MAX_WINDOWS {
        out.sort_by(|a, b| b.speech_s.total_cmp(&a.speech_s));
        out.truncate(MAX_WINDOWS);
        out.sort_by_key(|w| w.start);
    }
    out
}

/// Computes voiceprints for turns that are already known, for example a
/// project transcribed before voiceprints existed (or edited since).
pub struct VoiceEmbedder {
    model: EmbeddingModel,
}

impl VoiceEmbedder {
    /// Load from the speaker models' folder (as [`crate::Diarizer::load`]).
    pub fn load(models_dir: &Path) -> Result<Self> {
        let bundle = ModelBundle::from_dir(models_dir).with_context(|| format!("reading the speaker models in {}", models_dir.display()))?;
        let model = EmbeddingModel::with_mode(bundle.embedding_path(), speakrs::ExecutionMode::Cpu)
            .with_context(|| format!("loading the voice model from {}", models_dir.display()))?;
        Ok(VoiceEmbedder { model })
    }

    /// One voiceprint per speaker of `turns` (mono 16 kHz `samples`), from
    /// up to [`MAX_WINDOWS`] 10 s windows each. Speakers with too little
    /// speech have none. Where turns of different speakers overlap, neither
    /// counts there.
    pub fn voiceprints(&mut self, samples: &[f32], turns: &[Turn]) -> Result<BTreeMap<String, Voiceprint>> {
        let mut by_speaker: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
        for t in exclusive(turns) {
            by_speaker.entry(t.speaker.clone()).or_default().push((t.start, t.end));
        }
        let mut out = BTreeMap::new();
        let mut buf = vec![0.0f32; WINDOW_SAMPLES];
        for (speaker, mut spans) in by_speaker {
            // A speaker's own turns may overlap (app-supplied turns).
            spans.sort_by(|a, b| a.0.total_cmp(&b.0));
            spans.dedup_by(|b, a| {
                let joined = b.0 <= a.1;
                if joined {
                    a.1 = a.1.max(b.1);
                }
                joined
            });
            let speech: f64 = spans.iter().map(|(a, b)| b - a).sum();
            let mut mean = Mean::default();
            for w in windows_for(&spans, samples.len()) {
                let end = (w.start + WINDOW_SAMPLES).min(samples.len());
                buf.fill(0.0);
                buf[..end - w.start].copy_from_slice(&samples[w.start..end]);
                let e = self.model.embed_masked(&buf, &w.mask, None).context("computing a voiceprint")?;
                mean.add(e.iter().copied(), w.speech_s);
            }
            if let Some(v) = mean.finish(speech) {
                out.insert(speaker, v);
            }
        }
        Ok(out)
    }
}

/// `turns` sorted, with the stretches where two speakers' turns overlap cut out.
fn exclusive(turns: &[Turn]) -> Vec<Turn> {
    let mut sorted: Vec<Turn> = turns.iter().filter(|t| t.end > t.start).cloned().collect();
    sorted.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut out = Vec::with_capacity(sorted.len());
    for (i, t) in sorted.iter().enumerate() {
        let mut pieces = vec![(t.start, t.end)];
        for (j, o) in sorted.iter().enumerate() {
            if i == j || o.speaker == t.speaker || o.end <= t.start || o.start >= t.end {
                continue;
            }
            pieces = pieces
                .into_iter()
                .flat_map(|(a, b)| {
                    let mut keep = Vec::new();
                    if o.start > a {
                        keep.push((a, o.start.min(b)));
                    }
                    if o.end < b {
                        keep.push((o.end.max(a), b));
                    }
                    keep
                })
                .filter(|(a, b)| b > a)
                .collect();
        }
        out.extend(pieces.into_iter().map(|(start, end)| Turn { start, end, speaker: t.speaker.clone() }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(start: f64, end: f64, speaker: &str) -> Turn {
        Turn { start, end, speaker: speaker.into() }
    }

    #[test]
    fn the_mean_is_normalised_and_weighted() {
        let mut m = Mean::default();
        m.add([2.0, 0.0], 3.0); // normalised before adding
        m.add([0.0, 5.0], 1.0);
        m.add([f32::NAN, 1.0], 9.0); // ignored
        m.add([0.0, 0.0], 9.0); // ignored
        let v = m.finish(12.345).unwrap();
        let norm: f32 = v.vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
        assert!(v.vector[0] > v.vector[1] * 2.9 && v.vector[0] < v.vector[1] * 3.1, "{:?}", v.vector);
        assert_eq!(v.speech_s, 12.35);
        assert!(Mean::default().finish(1.0).is_none());
    }

    #[test]
    fn similarity_needs_the_same_model() {
        let a = Voiceprint { model: VOICEPRINT_MODEL.into(), vector: vec![1.0, 0.0], speech_s: 1.0 };
        let b = Voiceprint { vector: vec![1.0, 1.0], ..a.clone() };
        assert!((a.similarity(&b).unwrap() - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        assert_eq!(a.similarity(&Voiceprint { model: "other".into(), ..b }), None);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn windows_follow_the_speaker() {
        let sr = SAMPLE_RATE as usize;
        // Talks 0–4 s and 6–8 s (one window), then 30–55 s (three windows, the last short).
        let w = windows_for(&[(0.0, 4.0), (6.0, 8.0), (30.0, 55.0)], 60 * sr);
        let starts: Vec<f64> = w.iter().map(|w| w.start as f64 / sr as f64).collect();
        assert_eq!(starts, vec![0.0, 30.0, 40.0, 50.0]);
        assert!((w[0].speech_s - 6.0).abs() < 1e-9);
        let on = |w: &Window| w.mask.iter().filter(|&&m| m > 0.0).count();
        // 6 of 10 s are the speaker's: about 60 % of the frames.
        assert!((350..=360).contains(&on(&w[0])), "{}", on(&w[0]));
        assert_eq!(w[0].mask[MASK_FRAMES * 5 / 10], 0.0, "silent 4–6 s");
        assert_eq!(on(&w[1]), MASK_FRAMES);
        assert!((w[3].speech_s - 5.0).abs() < 1e-9);
        // A window running past the end of the audio is masked there.
        let w = windows_for(&[(55.0, 62.0)], 60 * sr);
        assert_eq!(w.len(), 1);
        assert!((w[0].speech_s - 5.0).abs() < 1e-9);
        assert!(w[0].mask[MASK_FRAMES - 1] == 0.0);
        // Only short bits: kept, since there's nothing better.
        assert_eq!(windows_for(&[(1.0, 1.4)], 10 * sr).len(), 1);
        // Short bits go when there are good windows.
        assert_eq!(windows_for(&[(0.0, 0.4), (20.0, 25.0)], 60 * sr).len(), 1);
        assert!(windows_for(&[], 10 * sr).is_empty());
    }

    #[test]
    fn long_speakers_are_capped_at_their_best_windows() {
        let sr = SAMPLE_RATE as usize;
        let turns: Vec<(f64, f64)> = (0..100).map(|i| (i as f64 * 10.0, i as f64 * 10.0 + if i % 2 == 0 { 9.0 } else { 3.0 })).collect();
        let w = windows_for(&turns, 1000 * sr);
        assert_eq!(w.len(), MAX_WINDOWS);
        assert!(w.iter().all(|w| w.speech_s >= 9.0 - 1e-9));
        assert!(w.windows(2).all(|p| p[0].start < p[1].start));
    }

    #[test]
    fn overlapping_speech_counts_for_no_one() {
        let ex = exclusive(&[t(0.0, 5.0, "A"), t(4.0, 8.0, "B"), t(6.0, 7.0, "B")]);
        let spans: Vec<(f64, f64, &str)> = ex.iter().map(|t| (t.start, t.end, t.speaker.as_str())).collect();
        assert_eq!(spans, vec![(0.0, 4.0, "A"), (5.0, 8.0, "B"), (6.0, 7.0, "B")]);
    }
}

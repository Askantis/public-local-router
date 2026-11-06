//! Speaker diarization with speakrs: segmentation-3.0, WeSpeaker embeddings,
//! PLDA and VBx clustering, as in pyannote's community-1 pipeline.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use speakrs::{ExecutionMode, OwnedDiarizationPipeline};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Turn {
    pub start: f64,
    pub end: f64,
    pub speaker: String,
}

/// Two speakers talking at once (Scribely beta's `detect_overlaps` shape).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Overlap {
    pub start: f64,
    pub end: f64,
    /// Sorted.
    pub speakers: Vec<String>,
    pub duration_s: f64,
}

/// Shorter overlaps are boundary jitter rather than people talking at once.
pub const MIN_OVERLAP_S: f64 = 0.5;

pub struct Diarizer {
    pipeline: OwnedDiarizationPipeline,
    overlaps: Vec<Overlap>,
}

/// The execution mode. Native CPU by default: on a three-speaker test it
/// matches pyannote community-1 exactly, while speakrs 0.7's Core ML
/// embeddings confused two similar voices on 24% of the speech (bench/RESULTS.md).
/// `LR_DIARIZE_MODE=coreml` or `coreml-fast` switches, for comparing.
pub fn default_mode() -> ExecutionMode {
    match std::env::var("LR_DIARIZE_MODE").as_deref() {
        Ok("coreml") => ExecutionMode::CoreMl,
        Ok("coreml-fast") => ExecutionMode::CoreMlFast,
        _ => ExecutionMode::Cpu,
    }
}

impl Diarizer {
    /// Load from a folder holding the files of `speakrs::models::required_model_files`.
    pub fn load(models_dir: &Path) -> Result<Self> {
        let pipeline = OwnedDiarizationPipeline::from_dir(models_dir, default_mode())
            .with_context(|| format!("loading the speaker models from {}", models_dir.display()))?;
        Ok(Diarizer { pipeline, overlaps: Vec::new() })
    }

    /// Who speaks when, with at most one speaker at a time (overlaps go to the
    /// louder speaker), like pyannote's `exclusive_speaker_diarization`.
    /// The overlaps of the speakers' own segments go to [`Diarizer::overlaps`].
    pub fn run(&mut self, audio: &[f32]) -> Result<Vec<Turn>> {
        self.overlaps.clear();
        let result = self.pipeline.run(audio).context("separating speakers")?;
        let turns: Vec<Turn> = result
            .exclusive_segments
            .into_iter()
            .map(|s| Turn { start: s.start, end: s.end, speaker: s.speaker })
            .collect();
        let raw: Vec<Turn> = result.segments.into_iter().map(|s| Turn { start: s.start, end: s.end, speaker: s.speaker }).collect();
        self.overlaps = name_like(&turns, detect_overlaps(&raw, MIN_OVERLAP_S));
        Ok(turns)
    }

    /// Where two speakers talked at once in the last [`Diarizer::run`],
    /// speakers named as [`crate::label_speakers`] names that run's turns
    /// ("Speaker 1", …).
    pub fn overlaps(&self) -> &[Overlap] {
        &self.overlaps
    }
}

/// Time ranges of at least `min_s` where two different speakers' segments
/// overlap, from segments that may overlap (before merging). Scribely beta's
/// `detect_overlaps`, sorted by start and end.
pub fn detect_overlaps(segments: &[Turn], min_s: f64) -> Vec<Overlap> {
    let mut sorted = segments.to_vec();
    sorted.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut out = Vec::new();
    for (i, a) in sorted.iter().enumerate() {
        for b in &sorted[i + 1..] {
            if b.start >= a.end {
                break;
            }
            if a.speaker == b.speaker {
                continue;
            }
            let (start, end) = (a.start.max(b.start), a.end.min(b.end));
            if end - start >= min_s {
                let mut speakers = vec![a.speaker.clone(), b.speaker.clone()];
                speakers.sort();
                out.push(Overlap { start: r2(start), end: r2(end), speakers, duration_s: r2(end - start) });
            }
        }
    }
    out.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.end.total_cmp(&b.end)));
    out
}

/// The speakers of `overlaps` named as `label_speakers(turns)` names them.
fn name_like(turns: &[Turn], overlaps: Vec<Overlap>) -> Vec<Overlap> {
    let labelled = crate::label_speakers(turns.to_vec());
    let mut sorted = turns.to_vec();
    sorted.sort_by(|a, b| a.start.total_cmp(&b.start));
    let name = |raw: &str| sorted.iter().zip(&labelled).find(|(t, _)| t.speaker == raw).map(|(_, l)| l.speaker.clone()).unwrap_or_else(|| raw.to_string());
    overlaps
        .into_iter()
        .map(|o| {
            let mut speakers: Vec<String> = o.speakers.iter().map(|s| name(s)).collect();
            speakers.sort();
            Overlap { speakers, ..o }
        })
        .collect()
}

fn r2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(start: f64, end: f64, speaker: &str) -> Turn {
        Turn { start, end, speaker: speaker.into() }
    }

    #[test]
    fn overlaps_like_the_beta() {
        let segs = [t(0.0, 5.0, "SPEAKER_01"), t(4.0, 6.0, "SPEAKER_00"), t(4.8, 5.1, "SPEAKER_02"), t(5.5, 7.0, "SPEAKER_01"), t(6.0, 6.2, "SPEAKER_01")];
        let o = detect_overlaps(&segs, MIN_OVERLAP_S);
        assert_eq!(o, vec![
            Overlap { start: 4.0, end: 5.0, speakers: vec!["SPEAKER_00".into(), "SPEAKER_01".into()], duration_s: 1.0 },
            Overlap { start: 5.5, end: 6.0, speakers: vec!["SPEAKER_00".into(), "SPEAKER_01".into()], duration_s: 0.5 },
        ]);
        // Named as the turns are: SPEAKER_01 speaks first.
        let named = name_like(&[t(0.0, 4.0, "SPEAKER_01"), t(4.0, 6.0, "SPEAKER_00")], o);
        assert_eq!(named[0].speakers, vec!["Speaker 1", "Speaker 2"]);
        assert!(detect_overlaps(&[], MIN_OVERLAP_S).is_empty());
    }
}

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

pub struct Diarizer {
    pipeline: OwnedDiarizationPipeline,
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
        Ok(Diarizer { pipeline })
    }

    /// Who speaks when, with at most one speaker at a time (overlaps go to the
    /// louder speaker), like pyannote's `exclusive_speaker_diarization`.
    pub fn run(&mut self, audio: &[f32]) -> Result<Vec<Turn>> {
        let result = self.pipeline.run(audio).context("separating speakers")?;
        Ok(result
            .exclusive_segments
            .into_iter()
            .map(|s| Turn { start: s.start, end: s.end, speaker: s.speaker })
            .collect())
    }
}

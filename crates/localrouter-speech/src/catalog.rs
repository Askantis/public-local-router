//! The speech models the engine knows, and fetching them into the standard
//! Hugging Face cache.

use std::path::PathBuf;

use anyhow::{Context, Result};
use localrouter_hf::Cache;

/// A Whisper model in ggml format, stored in a Hugging Face repository.
#[derive(Debug, Clone, Copy)]
pub struct WhisperModel {
    pub id: &'static str,
    pub display_name: &'static str,
    pub repo: &'static str,
    pub file: &'static str,
    pub revision: &'static str,
    pub approx_bytes: u64,
}

pub const WHISPER_TURBO_Q8: WhisperModel = WhisperModel {
    id: "whisper-large-v3-turbo-q8_0",
    display_name: "Whisper Large v3 Turbo",
    repo: "ggerganov/whisper.cpp",
    file: "ggml-large-v3-turbo-q8_0.bin",
    revision: "main",
    approx_bytes: 874_000_000,
};

pub const WHISPER_TURBO_F16: WhisperModel = WhisperModel {
    id: "whisper-large-v3-turbo",
    display_name: "Whisper Large v3 Turbo (full precision)",
    repo: "ggerganov/whisper.cpp",
    file: "ggml-large-v3-turbo.bin",
    revision: "main",
    approx_bytes: 1_624_000_000,
};

pub const WHISPER_MODELS: &[WhisperModel] = &[WHISPER_TURBO_Q8, WHISPER_TURBO_F16];

pub fn whisper_model(id: &str) -> Option<WhisperModel> {
    WHISPER_MODELS.iter().copied().find(|m| m.id == id)
}

/// The speaker-diarization models, pinned to the revision speakrs was built for.
pub fn speaker_model_files() -> Vec<String> {
    speakrs::models::required_model_files(crate::diarize::default_mode())
}

pub const SPEAKER_REPO: &str = speakrs::models::MODEL_REPOSITORY;
pub const SPEAKER_REVISION: &str = speakrs::models::MODEL_REVISION;

/// Path of a Whisper model in the cache, downloading it if needed.
pub fn ensure_whisper(cache: &Cache, model: WhisperModel, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<PathBuf> {
    cache
        .download(model.repo, model.file, model.revision, progress)
        .with_context(|| format!("getting {} from {}", model.file, model.repo))
}

/// Whether a Whisper model is already in the cache.
pub fn whisper_present(cache: &Cache, model: WhisperModel) -> Result<Option<PathBuf>> {
    Ok(cache.snapshot_path(model.repo, model.file, model.revision)?)
}

/// Folder holding the speaker models, downloading missing files.
pub fn ensure_speaker_models(cache: &Cache, progress: &mut dyn FnMut(&str, u64, Option<u64>)) -> Result<PathBuf> {
    let files = speaker_model_files();
    for f in &files {
        cache
            .download(SPEAKER_REPO, f, SPEAKER_REVISION, &mut |d, t| progress(f, d, t))
            .with_context(|| format!("getting {f} from {SPEAKER_REPO}"))?;
    }
    cache
        .snapshot_dir(SPEAKER_REPO, SPEAKER_REVISION)?
        .context("speaker models missing after download")
}

/// The speaker-model folder, if every file is in the cache.
pub fn speaker_models_present(cache: &Cache) -> Result<Option<PathBuf>> {
    for f in speaker_model_files() {
        if cache.snapshot_path(SPEAKER_REPO, &f, SPEAKER_REVISION)?.is_none() {
            return Ok(None);
        }
    }
    Ok(cache.snapshot_dir(SPEAKER_REPO, SPEAKER_REVISION)?)
}

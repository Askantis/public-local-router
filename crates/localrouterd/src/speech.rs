//! Hosting the speech engine: models load on first use and stay loaded; jobs
//! run one at a time on a blocking thread (PROTOCOL.md 11.4, 11.5).

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use localrouter_hf::Cache;

use localrouter_speech::{Asr, AsrOptions, Audio, Diarizer, Strategy, Turn, catalog};
use serde_json::{Value, json};

pub struct Speech {
    /// Set once the Hugging Face cache has been checked (that can wait for the
    /// user to allow access to a removable drive).
    cache: std::sync::RwLock<Option<Cache>>,
    asr: Mutex<Option<(String, Arc<Asr>)>>,
    diarizer: Mutex<Option<Diarizer>>,
    /// One job at a time: the GPU and memory are shared.
    pub job: tokio::sync::Mutex<()>,
}

impl Speech {
    pub fn new() -> Self {
        Speech { cache: std::sync::RwLock::new(None), asr: Mutex::new(None), diarizer: Mutex::new(None), job: tokio::sync::Mutex::new(()) }
    }

    pub fn set_cache(&self, cache: Option<Cache>) {
        *self.cache.write().unwrap() = cache;
    }

    pub fn cache(&self) -> Option<Cache> {
        self.cache.read().unwrap().clone()
    }

    /// Speech models present in the cache: `(model id, capability, display name, bytes)`.
    pub fn installed(&self) -> Vec<(String, localrouter_core::Capability, String, u64)> {
        let Some(cache) = self.cache() else { return vec![] };
        let cache = &cache;
        let mut out = Vec::new();
        for m in catalog::WHISPER_MODELS {
            if let Ok(Some(path)) = catalog::whisper_present(cache, *m) {
                let size = std::fs::metadata(&path).map(|md| md.len()).unwrap_or(m.approx_bytes);
                out.push((catalog::whisper_model_id(m), localrouter_core::Capability::Transcription, m.display_name.to_string(), size));
            }
        }
        if let Ok(Some(_)) = catalog::speaker_models_present(cache) {
            out.push((catalog::speaker_model_id(), localrouter_core::Capability::Diarization, "Speaker diarization (pyannote community-1)".into(), 183_000_000));
        }
        out
    }

    fn asr_for(&self, model_id: &str) -> Result<Arc<Asr>> {
        let mut slot = self.asr.lock().unwrap();
        if let Some((id, asr)) = slot.as_ref()
            && id == model_id {
                return Ok(asr.clone());
            }
        let cache = self.cache().context("the Hugging Face cache isn't available")?;
        let model = catalog::whisper_by_model_id(model_id).context("not a speech model Local Router knows")?;
        let path = catalog::whisper_present(&cache, model)?.context("model isn't downloaded")?;
        // Free the old model before loading the new one.
        *slot = None;
        let asr = Arc::new(Asr::load(&path, catalog::aheads_for(&model))?);
        *slot = Some((model_id.to_string(), asr.clone()));
        Ok(asr)
    }

    fn with_diarizer<T>(&self, f: impl FnOnce(&mut Diarizer) -> Result<T>) -> Result<T> {
        let mut slot = self.diarizer.lock().unwrap();
        if slot.is_none() {
            let cache = self.cache().context("the Hugging Face cache isn't available")?;
            let dir = catalog::speaker_models_present(&cache)?.context("speaker models aren't downloaded")?;
            *slot = Some(Diarizer::load(&dir)?);
        }
        f(slot.as_mut().unwrap())
    }

    /// Whether Local Router runs this model itself (rather than Ollama).
    pub fn knows(model_id: &str) -> bool {
        catalog::whisper_by_model_id(model_id).is_some() || model_id == catalog::speaker_model_id()
    }

    /// Load a speech model now, so the first request doesn't wait for it.
    pub fn preload(&self, model_id: &str) -> Result<()> {
        if model_id == catalog::speaker_model_id() {
            self.with_diarizer(|_| Ok(()))
        } else {
            self.asr_for(model_id).map(|_| ())
        }
    }

    /// Free a speech model's memory.
    pub fn unload(&self, model_id: &str) {
        if model_id == catalog::speaker_model_id() {
            *self.diarizer.lock().unwrap() = None;
        } else {
            let mut slot = self.asr.lock().unwrap();
            if slot.as_ref().is_some_and(|(id, _)| id == model_id) {
                *slot = None;
            }
        }
    }

    pub fn loaded(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some((id, _)) = self.asr.lock().unwrap().as_ref() {
            out.push(id.clone());
        }
        if self.diarizer.lock().unwrap().is_some() {
            out.push(catalog::speaker_model_id());
        }
        out
    }

    /// Who speaks when, speakers renamed `S1`, `S2`, … by first appearance.
    pub fn diarize(&self, audio: &Audio) -> Result<Vec<Turn>> {
        let turns = self.with_diarizer(|d| d.run(&audio.samples))?;
        Ok(relabel(localrouter_speech::label_speakers(turns)))
    }

    pub fn transcribe(&self, audio: &Audio, req: &TranscribeRequest) -> Result<localrouter_speech::Transcript> {
        self.transcribe_streaming(audio, req, &mut |_| {})
    }

    /// Whether `model_id` is the speech-to-text model in memory now.
    pub fn asr_is(&self, model_id: &str) -> bool {
        self.asr.lock().unwrap().as_ref().is_some_and(|(id, _)| id == model_id)
    }

    /// [`Speech::transcribe`], reporting lines and final turns (with short
    /// speaker labels) as they're ready. Speakers are separated while the
    /// words are written ([`localrouter_speech::transcribe_live`]).
    pub fn transcribe_streaming(
        &self,
        audio: &Audio,
        req: &TranscribeRequest,
        on: &mut dyn FnMut(localrouter_speech::Update<'_>),
    ) -> Result<localrouter_speech::Transcript> {
        use localrouter_speech::{Line, Speakers, TranscriptTurn, Update};
        let asr = self.asr_for(&req.model_id)?;
        let opts = AsrOptions { language: req.language.clone(), prompt: req.prompt.clone(), threads: 0 };
        // Load the speaker models here, so a missing one fails before any work.
        let mut slot = None;
        if req.segments.is_none() && req.diarize {
            self.with_diarizer(|_| Ok(()))?;
            slot = Some(self.diarizer.lock().unwrap());
        }
        let speakers = match (&req.segments, slot.as_mut()) {
            (Some(s), _) => Speakers::Known(s.clone()),
            (None, Some(guard)) => {
                let d = guard.as_mut().context("speaker models aren't loaded")?;
                Speakers::Separate(Box::new(move || d.run(&audio.samples)))
            }
            (None, None) => Speakers::None,
        };
        let mut relabelled = |u: Update<'_>| match u {
            Update::Line(l) => on(Update::Line(&Line { speaker: l.speaker.as_deref().map(short_label), ..l.clone() })),
            Update::Speakers(s) => on(Update::Speakers(&s.iter().map(|x| x.as_deref().map(short_label)).collect::<Vec<_>>())),
            Update::Turn(t) => on(Update::Turn(&TranscriptTurn { speaker: short_label(&t.speaker), ..t.clone() })),
            other => on(other),
        };
        let mut t = localrouter_speech::transcribe_live(audio, &asr, speakers, &opts, Strategy::Windows, &mut |_, _| {}, &mut relabelled)?;
        for turn in &mut t.turns {
            turn.speaker = short_label(&turn.speaker);
        }
        t.speakers = t.speakers.iter().map(|s| short_label(s)).collect();
        Ok(t)
    }
}

/// "Speaker 3" → "S3"; app-supplied labels stay as they are.
fn short_label(s: &str) -> String {
    s.strip_prefix("Speaker ").map(|n| format!("S{n}")).unwrap_or_else(|| s.to_string())
}

fn relabel(turns: Vec<Turn>) -> Vec<Turn> {
    turns.into_iter().map(|t| Turn { speaker: short_label(&t.speaker), ..t }).collect()
}

pub struct TranscribeRequest {
    pub model_id: String,
    pub language: Option<String>,
    pub prompt: Option<String>,
    pub diarize: bool,
    pub segments: Option<Vec<Turn>>,
    pub offset: f64,
    pub words: bool,
}

/// The answer in one of the formats of PROTOCOL.md 11.4.
fn words_of(turn: &localrouter_speech::TranscriptTurn, off: f64) -> Vec<Value> {
    turn.words
        .iter()
        .map(|w| json!({ "word": w.word, "start": r3(w.start + off), "end": r3(w.end + off), "probability": r3(w.probability as f64) }))
        .collect()
}

/// One segment of `verbose_json` / `lr_speakers_json`, also sent alone when streaming.
pub fn segment_json(i: usize, turn: &localrouter_speech::TranscriptTurn, format: &str, req: &TranscribeRequest) -> Value {
    let off = req.offset;
    let mut s = json!({ "id": i, "start": r3(turn.start + off), "end": r3(turn.end + off), "text": turn.text });
    if format == "lr_speakers_json" {
        s["speaker"] = json!(turn.speaker);
    }
    if req.words {
        s["words"] = json!(words_of(turn, off));
    }
    if let Some(p) = turn.no_speech_prob {
        s["no_speech_prob"] = json!(r3(p as f64));
    }
    s
}

/// A line for `lr.transcript.line`: `{ index, start, end, text, speaker }`,
/// `speaker` only with `lr_speakers_json` (`null` while not known yet).
pub fn line_json(l: &localrouter_speech::Line, format: &str, req: &TranscribeRequest) -> Value {
    let mut v = json!({ "index": l.index, "start": r3(l.start + req.offset), "end": r3(l.end + req.offset), "text": l.text });
    if format == "lr_speakers_json" {
        v["speaker"] = json!(l.speaker);
    }
    v
}

pub fn render(t: &localrouter_speech::Transcript, format: &str, req: &TranscribeRequest) -> (Value, Option<String>) {
    let off = req.offset;
    let text = t.turns.iter().map(|x| x.text.as_str()).collect::<Vec<_>>().join(" ");
    let segments: Vec<Value> = t.turns.iter().enumerate().map(|(i, turn)| segment_json(i, turn, format, req)).collect();
    match format {
        "text" => (Value::Null, Some(text)),
        "srt" | "vtt" => (Value::Null, Some(subtitles(t, format == "vtt", off))),
        "verbose_json" | "lr_speakers_json" => {
            let mut v = json!({
                "task": "transcribe",
                "language": t.language.clone().unwrap_or_default(),
                "duration": r3(t.duration_s),
                "text": text,
                "segments": segments,
            });
            if req.words {
                v["words"] = json!(t.turns.iter().flat_map(|turn| words_of(turn, off)).collect::<Vec<_>>());
            }
            if format == "lr_speakers_json" {
                v["speakers"] = json!(t.speakers.iter().map(|s| json!({ "id": s })).collect::<Vec<_>>());
                v["overlaps"] = json!([]);
            }
            (v, None)
        }
        _ => (json!({ "text": text }), None),
    }
}

fn r3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

fn subtitles(t: &localrouter_speech::Transcript, vtt: bool, off: f64) -> String {
    let stamp = |s: f64| {
        let ms = ((s + off).max(0.0) * 1000.0).round() as u64;
        let sep = if vtt { '.' } else { ',' };
        format!("{:02}:{:02}:{:02}{sep}{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
    };
    let mut out = String::from(if vtt { "WEBVTT\n\n" } else { "" });
    for (i, turn) in t.turns.iter().enumerate() {
        if !vtt {
            out.push_str(&format!("{}\n", i + 1));
        }
        out.push_str(&format!("{} --> {}\n{}\n\n", stamp(turn.start), stamp(turn.end), turn.text));
    }
    out
}

pub fn render_diarization(turns: &[Turn], duration: f64, format: &str, file_id: &str) -> (Value, Option<String>) {
    if format == "rttm" {
        let lines: String = turns
            .iter()
            .map(|t| format!("SPEAKER {file_id} 1 {:.3} {:.3} <NA> <NA> {} <NA> <NA>\n", t.start, t.end - t.start, t.speaker))
            .collect();
        return (Value::Null, Some(lines));
    }
    let mut speakers: Vec<&str> = Vec::new();
    for t in turns {
        if !speakers.contains(&t.speaker.as_str()) {
            speakers.push(&t.speaker);
        }
    }
    (
        json!({
            "duration": r3(duration),
            "speakers": speakers.iter().map(|s| json!({ "id": s })).collect::<Vec<_>>(),
            "segments": turns.iter().map(|t| json!({ "start": r3(t.start), "end": r3(t.end), "speaker": t.speaker })).collect::<Vec<_>>(),
            "overlaps": [],
        }),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use localrouter_speech::{Transcript, TranscriptTurn, Word};

    fn sample() -> Transcript {
        let w = |word: &str, s: f64, e: f64| Word { word: word.into(), start: s, end: e, probability: 0.9 };
        Transcript {
            duration_s: 4.0,
            language: Some("de".into()),
            speakers: vec!["S1".into(), "S2".into()],
            turns: vec![
                TranscriptTurn { start: 0.3, end: 1.0, speaker: "S1".into(), text: "Hoi Thomas".into(), words: vec![w("Hoi", 0.3, 0.6), w("Thomas", 0.6, 1.0)], no_speech_prob: Some(0.012) },
                TranscriptTurn { start: 1.5, end: 2.0, speaker: "S2".into(), text: "Grüezi".into(), words: vec![w("Grüezi", 1.5, 2.0)], no_speech_prob: None },
            ],
        }
    }

    fn req(words: bool, offset: f64) -> TranscribeRequest {
        TranscribeRequest { model_id: String::new(), language: None, prompt: None, diarize: true, segments: None, offset, words }
    }

    #[test]
    fn formats_match_the_protocol() {
        let (v, _) = render(&sample(), "lr_speakers_json", &req(true, 0.0));
        assert_eq!(v["speakers"], json!([{ "id": "S1" }, { "id": "S2" }]));
        assert_eq!(v["segments"][1]["speaker"], "S2");
        assert_eq!(v["words"].as_array().unwrap().len(), 3);
        let (v, _) = render(&sample(), "verbose_json", &req(false, 10.0));
        assert!(v.get("words").is_none() && v["segments"][0].get("speaker").is_none());
        assert_eq!(v["segments"][0]["start"], 10.3, "lr_offset shifts every time");
        let (v, _) = render(&sample(), "json", &req(false, 0.0));
        assert_eq!(v, json!({ "text": "Hoi Thomas Grüezi" }));
        let (_, srt) = render(&sample(), "srt", &req(false, 0.0));
        assert!(srt.unwrap().starts_with("1\n00:00:00,300 --> 00:00:01,000\nHoi Thomas\n"));
        let (_, vtt) = render(&sample(), "vtt", &req(false, 0.0));
        assert!(vtt.unwrap().starts_with("WEBVTT\n\n00:00:00.300 --> 00:00:01.000\n"));
    }

    #[test]
    fn rttm_lines() {
        let turns = vec![Turn { start: 0.5, end: 2.0, speaker: "S1".into() }];
        let (_, rttm) = render_diarization(&turns, 3.0, "rttm", "upload");
        assert_eq!(rttm.unwrap(), "SPEAKER upload 1 0.500 1.500 <NA> <NA> S1 <NA> <NA>\n");
    }
}

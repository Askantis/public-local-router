//! The speech engine shared by Local Router's service and by apps running
//! standalone: Whisper transcription with word timestamps (whisper.cpp through
//! whisper-rs, with DTW alignment) and speaker diarization (speakrs, the
//! pyannote community-1 pipeline in Rust). Models come from the standard
//! Hugging Face cache ([`catalog`]).
//!
//! [`transcribe`] runs the same pipeline as Scribely beta: separate speakers
//! over the whole file, merge a speaker's turns across short gaps, split turns
//! longer than 28 s, then transcribe each turn with word timestamps.

pub mod asr;
pub mod catalog;
pub mod diarize;

use std::collections::HashMap;

use anyhow::Result;
use serde::Serialize;

pub use asr::{Aheads, Asr, AsrOptions, AsrSegment, Word};
pub use diarize::{Diarizer, Turn};
pub use localrouter_audio::{Audio, SAMPLE_RATE};

/// Gap between two turns of the same speaker that still counts as one turn.
pub const MERGE_GAP_S: f64 = 0.75;
/// Longest turn handed to Whisper in one piece (its window is 30 s).
pub const MAX_TURN_S: f64 = 28.0;
/// Turns shorter than this are skipped: Whisper invents text for them.
pub const MIN_TURN_S: f64 = 0.3;
/// Audio added before and after a turn so its first and last words aren't cut.
pub const TURN_PAD_S: f64 = 0.1;
/// Window and overlap used when speakers aren't separated.
pub const NO_DIAR_WINDOW_S: f64 = 25.0;
pub const NO_DIAR_OVERLAP_S: f64 = 0.5;

#[derive(Debug, Clone, Serialize)]
pub struct TranscriptTurn {
    pub start: f64,
    pub end: f64,
    pub speaker: String,
    pub text: String,
    pub words: Vec<Word>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Transcript {
    pub duration_s: f64,
    pub language: Option<String>,
    pub speakers: Vec<String>,
    pub turns: Vec<TranscriptTurn>,
}

/// Progress of [`transcribe`]: `(steps done, steps total)`.
pub type Progress<'a> = &'a mut dyn FnMut(usize, usize);

/// Receives each turn as soon as it's final, in order.
pub type TurnSink<'a> = &'a mut dyn FnMut(&TranscriptTurn);

/// How speech is cut up for Whisper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strategy {
    /// Transcribe windows of up to [`MAX_TURN_S`] that span speaker changes,
    /// then give each word to the speaker talking at its midpoint. Whisper
    /// sees context, so very short turns aren't transcribed alone (where it
    /// tends to invent text), and there are fewer windows to decode.
    #[default]
    Windows,
    /// Transcribe each speaker turn on its own, as Scribely beta does.
    Turns,
}

/// Transcribe `audio` with speakers separated by `diarizer`, or in fixed
/// windows labelled "Speaker 1" without one.
pub fn transcribe(
    audio: &Audio,
    asr: &Asr,
    diarizer: Option<&mut Diarizer>,
    opts: &AsrOptions,
    progress: Progress<'_>,
) -> Result<Transcript> {
    transcribe_with(audio, asr, diarizer, opts, Strategy::default(), progress)
}

pub fn transcribe_with(
    audio: &Audio,
    asr: &Asr,
    diarizer: Option<&mut Diarizer>,
    opts: &AsrOptions,
    strategy: Strategy,
    progress: Progress<'_>,
) -> Result<Transcript> {
    let speaker_turns = match diarizer {
        Some(d) => Some(merge_same_speaker(&label_speakers(d.run(&audio.samples)?))),
        None => None,
    };
    transcribe_turns(audio, asr, speaker_turns, opts, strategy, progress)
}

/// Transcribe with speaker turns that are already known, for example from an
/// app that separated speakers itself (PROTOCOL.md `lr_segments`). Without
/// turns the whole file is transcribed as "Speaker 1".
pub fn transcribe_turns(
    audio: &Audio,
    asr: &Asr,
    speaker_turns: Option<Vec<Turn>>,
    opts: &AsrOptions,
    strategy: Strategy,
    progress: Progress<'_>,
) -> Result<Transcript> {
    transcribe_turns_streaming(audio, asr, speaker_turns, opts, strategy, progress, &mut |_| {})
}

/// [`transcribe_turns`] that hands every turn to `sink` once it's final.
/// Without speakers that is after each window; with [`Strategy::Windows`]
/// a speaker's turn can continue into the next window, so turns come at
/// the end.
pub fn transcribe_turns_streaming(
    audio: &Audio,
    asr: &Asr,
    speaker_turns: Option<Vec<Turn>>,
    opts: &AsrOptions,
    strategy: Strategy,
    progress: Progress<'_>,
    sink: TurnSink<'_>,
) -> Result<Transcript> {
    let duration = audio.duration_s();
    if strategy == Strategy::Windows
        && let Some(turns) = speaker_turns {
            let t = transcribe_windows(audio, asr, &turns, opts, progress)?;
            t.turns.iter().for_each(&mut *sink);
            return Ok(t);
        }
    let diarized = speaker_turns.is_some();
    let turns = match speaker_turns {
        Some(t) => split_long(&t),
        None => fixed_windows(duration),
    };
    let total = turns.len();
    let mut out = Vec::with_capacity(total);
    for (i, turn) in turns.iter().enumerate() {
        progress(i, total);
        if turn.end - turn.start < MIN_TURN_S {
            continue;
        }
        let start = (turn.start - TURN_PAD_S).max(0.0);
        let end = (turn.end + TURN_PAD_S).min(duration);
        if std::env::var_os("LR_DEBUG_TOKENS").is_some() {
            eprintln!("TURN\t{i}\t{start:.3}\t{end:.3}\t{}", turn.speaker);
        }
        let segments = asr.transcribe(audio.slice(start, end), start, opts)?;
        if !diarized {
            // No speakers: keep Whisper's own segments (sentence-like), and
            // drop the overlap of consecutive windows.
            let from = out.last().map(|t: &TranscriptTurn| t.end).unwrap_or(0.0);
            for s in segments.into_iter().filter(|s| !s.text.trim().is_empty() && s.end > from + 0.05) {
                let words: Vec<Word> = s.words.into_iter().filter(|w| w.start >= from).collect();
                let text = if words.is_empty() { s.text.trim().to_string() } else { join_text(words.iter().map(|w| w.word.as_str())) };
                out.push(TranscriptTurn { start: s.start.max(from), end: s.end, speaker: turn.speaker.clone(), text, words });
                sink(out.last().unwrap());
            }
            continue;
        }
        let text = join_text(segments.iter().map(|s| s.text.as_str()));
        if text.is_empty() {
            continue;
        }
        let words = segments.into_iter().flat_map(|s| s.words).collect();
        out.push(TranscriptTurn { start: turn.start, end: turn.end, speaker: turn.speaker.clone(), text, words });
        sink(out.last().unwrap());
    }
    progress(total, total);
    let mut speakers: Vec<String> = Vec::new();
    for t in &out {
        if !speakers.contains(&t.speaker) {
            speakers.push(t.speaker.clone());
        }
    }
    Ok(Transcript { duration_s: duration, language: opts.language.clone(), speakers, turns: out })
}

/// [`Strategy::Windows`]: pack consecutive speaker turns into windows of at
/// most [`MAX_TURN_S`] (cut between turns, or inside a turn that is longer),
/// transcribe each window, then hand every word to the speaker whose turn
/// overlaps it most, and group consecutive words of one speaker into turns.
fn transcribe_windows(
    audio: &Audio,
    asr: &Asr,
    turns: &[Turn],
    opts: &AsrOptions,
    progress: Progress<'_>,
) -> Result<Transcript> {
    let duration = audio.duration_s();
    let windows = pack_windows(&split_long(turns));
    let total = windows.len();
    let mut words: Vec<Word> = Vec::new();
    for (i, &(ws, we)) in windows.iter().enumerate() {
        progress(i, total);
        let start = (ws - TURN_PAD_S).max(0.0);
        let end = (we + TURN_PAD_S).min(duration);
        for seg in asr.transcribe(audio.slice(start, end), start, opts)? {
            words.extend(seg.words);
        }
    }
    progress(total, total);
    let mut out: Vec<TranscriptTurn> = Vec::new();
    for w in words {
        let speaker = speaker_at(turns, w.start, w.end);
        match out.last_mut() {
            Some(t) if t.speaker == speaker => {
                t.end = t.end.max(w.end);
                t.text.push(' ');
                t.text.push_str(&w.word);
                t.words.push(w);
            }
            _ => out.push(TranscriptTurn {
                start: w.start,
                end: w.end,
                speaker,
                text: w.word.clone(),
                words: vec![w],
            }),
        }
    }
    let mut speakers: Vec<String> = Vec::new();
    for t in &out {
        if !speakers.contains(&t.speaker) {
            speakers.push(t.speaker.clone());
        }
    }
    Ok(Transcript { duration_s: duration, language: opts.language.clone(), speakers, turns: out })
}

/// Greedily join consecutive turns into windows no longer than [`MAX_TURN_S`].
fn pack_windows(turns: &[Turn]) -> Vec<(f64, f64)> {
    let mut windows: Vec<(f64, f64)> = Vec::new();
    for t in turns {
        match windows.last_mut() {
            Some(w) if t.end - w.0 <= MAX_TURN_S => w.1 = w.1.max(t.end),
            _ => windows.push((t.start, t.end)),
        }
    }
    windows
}

/// The speaker whose turns overlap `[start, end]` most; the nearest turn's
/// speaker if none overlaps (a word in a gap between turns).
fn speaker_at(turns: &[Turn], start: f64, end: f64) -> String {
    let mid = (start + end) / 2.0;
    let mut best: Option<(&str, f64)> = None;
    for t in turns {
        let overlap = (end.min(t.end) - start.max(t.start)).max(0.0);
        // A word with zero length still belongs to the turn its midpoint is in.
        let score = if overlap > 0.0 { overlap } else if (t.start..=t.end).contains(&mid) { 1e-6 } else { 0.0 };
        if score > 0.0 && best.is_none_or(|(_, s)| score > s) {
            best = Some((&t.speaker, score));
        }
    }
    if let Some((s, _)) = best {
        return s.to_string();
    }
    turns
        .iter()
        .min_by(|a, b| dist(a, mid).total_cmp(&dist(b, mid)))
        .map(|t| t.speaker.clone())
        .unwrap_or_else(|| "Speaker 1".into())
}

fn dist(t: &Turn, x: f64) -> f64 {
    if x < t.start { t.start - x } else if x > t.end { x - t.end } else { 0.0 }
}

fn join_text<'a>(parts: impl Iterator<Item = &'a str>) -> String {
    parts.map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ")
}

/// Rename speakers "Speaker 1", "Speaker 2", … in the order they first speak.
pub fn label_speakers(turns: Vec<Turn>) -> Vec<Turn> {
    let mut sorted = turns;
    sorted.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut names: HashMap<String, String> = HashMap::new();
    sorted
        .into_iter()
        .map(|t| {
            let n = names.len() + 1;
            let speaker = names.entry(t.speaker.clone()).or_insert_with(|| format!("Speaker {n}")).clone();
            Turn { speaker, ..t }
        })
        .collect()
}

/// Join consecutive turns of the same speaker when the gap is at most
/// [`MERGE_GAP_S`] (Scribely beta's `merge_adjacent_same_speaker`).
pub fn merge_same_speaker(turns: &[Turn]) -> Vec<Turn> {
    let mut sorted = turns.to_vec();
    sorted.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut merged: Vec<Turn> = Vec::new();
    for t in sorted {
        match merged.last_mut() {
            Some(last) if last.speaker == t.speaker && t.start - last.end <= MERGE_GAP_S => {
                last.end = last.end.max(t.end);
            }
            _ => merged.push(t),
        }
    }
    merged
}

/// Cut turns longer than [`MAX_TURN_S`] into pieces (beta's `split_long_segments`).
pub fn split_long(turns: &[Turn]) -> Vec<Turn> {
    let mut out = Vec::new();
    for t in turns {
        let mut cur = t.start;
        while t.end - cur > MAX_TURN_S {
            out.push(Turn { start: cur, end: cur + MAX_TURN_S, speaker: t.speaker.clone() });
            cur += MAX_TURN_S;
        }
        out.push(Turn { start: cur, end: t.end, speaker: t.speaker.clone() });
    }
    out
}

fn fixed_windows(duration: f64) -> Vec<Turn> {
    let mut out = Vec::new();
    let mut start = 0.0;
    while start < duration {
        let end = (start + NO_DIAR_WINDOW_S).min(duration);
        out.push(Turn { start, end, speaker: "Speaker 1".into() });
        if end >= duration {
            break;
        }
        start = end - NO_DIAR_OVERLAP_S;
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
    fn merges_like_the_beta() {
        let merged = merge_same_speaker(&[t(0.0, 1.0, "A"), t(1.7, 2.0, "A"), t(2.9, 3.0, "A"), t(3.1, 4.0, "B")]);
        assert_eq!(merged.len(), 3);
        assert_eq!((merged[0].start, merged[0].end), (0.0, 2.0));
        assert_eq!((merged[1].start, merged[1].end), (2.9, 3.0));
        assert_eq!(merged[2].speaker, "B");
    }

    #[test]
    fn splits_like_the_beta() {
        let parts = split_long(&[t(10.0, 70.0, "A"), t(70.0, 80.0, "B")]);
        let spans: Vec<(f64, f64)> = parts.iter().map(|p| (p.start, p.end)).collect();
        assert_eq!(spans, vec![(10.0, 38.0), (38.0, 66.0), (66.0, 70.0), (70.0, 80.0)]);
    }

    #[test]
    fn labels_by_first_appearance() {
        let labelled = label_speakers(vec![t(5.0, 6.0, "SPEAKER_01"), t(0.0, 1.0, "SPEAKER_07"), t(7.0, 8.0, "SPEAKER_07")]);
        let names: Vec<&str> = labelled.iter().map(|x| x.speaker.as_str()).collect();
        assert_eq!(names, ["Speaker 1", "Speaker 2", "Speaker 1"]);
    }

    #[test]
    fn packs_turns_into_windows_up_to_the_limit() {
        let w = pack_windows(&[t(0.0, 10.0, "A"), t(10.5, 20.0, "B"), t(20.2, 27.0, "A"), t(27.5, 35.0, "B")]);
        assert_eq!(w, vec![(0.0, 27.0), (27.5, 35.0)]);
    }

    #[test]
    fn words_go_to_the_speaker_talking() {
        let turns = [t(0.0, 2.0, "A"), t(2.0, 4.0, "B")];
        assert_eq!(speaker_at(&turns, 0.5, 0.9), "A");
        assert_eq!(speaker_at(&turns, 1.8, 2.6), "B"); // mostly in B's turn
        assert_eq!(speaker_at(&turns, 3.0, 3.0), "B"); // zero length, inside B
        assert_eq!(speaker_at(&turns, 5.0, 5.4), "B"); // after the last turn: nearest
    }

    #[test]
    fn windows_without_diarization_overlap_and_cover_the_file() {
        let w = fixed_windows(60.0);
        assert_eq!(w.first().unwrap().start, 0.0);
        assert_eq!(w.last().unwrap().end, 60.0);
        assert!(w.windows(2).all(|p| (p[0].end - p[1].start - NO_DIAR_OVERLAP_S).abs() < 1e-9));
        assert_eq!(fixed_windows(10.0).len(), 1);
    }
}

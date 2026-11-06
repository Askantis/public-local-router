//! The speech engine shared by Local Router's service and by apps running
//! standalone: Whisper transcription with word timestamps (whisper.cpp through
//! whisper-rs, with DTW alignment) and speaker diarization (speakrs, the
//! pyannote community-1 pipeline in Rust). Models come from the standard
//! Hugging Face cache ([`catalog`]).
//!
//! [`transcribe`] separates speakers over the whole file (merging a
//! speaker's turns across short gaps) while Whisper writes the words in
//! windows of up to 28 s, then gives each word to the speaker talking.
//! [`transcribe_live`] reports lines and turns as they're ready.

pub mod asr;
pub mod catalog;
pub mod diarize;
pub mod voice;

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use serde::Serialize;

pub use asr::{Aheads, Asr, AsrOptions, AsrSegment, Word};
pub use diarize::{Diarizer, Overlap, Turn, detect_overlaps};
pub use voice::{VOICEPRINT_DIM, VOICEPRINT_MODEL, VoiceEmbedder, Voiceprint};
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
    /// Whisper's no-speech probability for this segment, when the turn is one
    /// Whisper segment (no speakers); `None` for turns built from words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_speech_prob: Option<f32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Transcript {
    pub duration_s: f64,
    pub language: Option<String>,
    pub speakers: Vec<String>,
    pub turns: Vec<TranscriptTurn>,
    /// Where two speakers talk at once, when the caller's [`Diarizer`] knows
    /// ([`Diarizer::overlaps`] after the transcription); empty otherwise.
    pub overlaps: Vec<Overlap>,
    /// Each speaker's voiceprint, by speaker, when the caller asked for them
    /// ([`Diarizer::voiceprints`] after the transcription); empty otherwise.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub voiceprints: BTreeMap<String, Voiceprint>,
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
    let speakers = match diarizer {
        Some(d) => Speakers::Separate(Box::new(move || d.run(&audio.samples))),
        None => Speakers::None,
    };
    transcribe_live(audio, asr, speakers, opts, strategy, progress, &mut |_| {})
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
pub fn transcribe_turns_streaming(
    audio: &Audio,
    asr: &Asr,
    speaker_turns: Option<Vec<Turn>>,
    opts: &AsrOptions,
    strategy: Strategy,
    progress: Progress<'_>,
    sink: TurnSink<'_>,
) -> Result<Transcript> {
    let speakers = match speaker_turns {
        Some(t) => Speakers::Known(t),
        None => Speakers::None,
    };
    transcribe_live(audio, asr, speakers, opts, strategy, progress, &mut |u| {
        if let Update::Turn(t) = u {
            sink(t)
        }
    })
}

/// A line as Whisper wrote it (one of its segments), sent as soon as it's
/// decoded, for showing progress. Lines are numbered from 0 in order; the
/// final turns ([`Update::Turn`]) may cut and join them differently.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Line {
    pub index: usize,
    pub start: f64,
    pub end: f64,
    pub text: String,
    /// The speaker talking most of the line; `None` while speakers aren't
    /// known yet, and without speakers.
    pub speaker: Option<String>,
}

/// What [`transcribe_live`] reports as it goes.
#[derive(Debug)]
pub enum Update<'a> {
    /// A line, as soon as it's decoded.
    Line(&'a Line),
    /// Speakers became known: the speaker of every line sent so far, by
    /// index. Lines sent after this come with their speaker.
    Speakers(&'a [Option<String>]),
    /// All words are written; the speakers are still being separated.
    WaitingForSpeakers,
    /// A turn that is final, in order. Turns don't change once sent.
    Turn(&'a TranscriptTurn),
}

/// Who speaks when, for [`transcribe_live`].
pub enum Speakers<'a> {
    /// Not separated: everything is "Speaker 1".
    None,
    /// Turns that are already known (an app's `lr_segments`).
    Known(Vec<Turn>),
    /// Separate them (a [`Diarizer::run`] over the whole file) on another
    /// thread while the words are written. The turns are labelled and merged
    /// here.
    Separate(Box<dyn FnOnce() -> Result<Vec<Turn>> + Send + 'a>),
}

/// Transcribe, reporting every line as soon as Whisper wrote it and every
/// turn once it's final.
///
/// With [`Speakers::Separate`] and [`Strategy::Windows`], speakers are
/// separated (on the CPU) while Whisper writes (on the GPU): the first lines
/// come after the first window instead of after the speaker separation, and
/// the whole job takes about as long as the slower of the two instead of
/// both. Whisper then reads the file in windows of up to [`MAX_TURN_S`] cut
/// at the quietest moment near their end, and each word goes to the speaker
/// talking at that moment, as with known turns.
pub fn transcribe_live(
    audio: &Audio,
    asr: &Asr,
    speakers: Speakers<'_>,
    opts: &AsrOptions,
    strategy: Strategy,
    progress: Progress<'_>,
    on: &mut dyn FnMut(Update<'_>),
) -> Result<Transcript> {
    let speakers = match (speakers, strategy) {
        (Speakers::Separate(run), Strategy::Windows) => return transcribe_separating(audio, asr, run, opts, progress, on),
        // Per turn, the turns are needed before the first window.
        (Speakers::Separate(run), Strategy::Turns) => Some(prepare_turns(run()?)),
        (Speakers::Known(t), _) => Some(t),
        (Speakers::None, _) => None,
    };
    match (speakers, strategy) {
        (Some(turns), Strategy::Windows) => transcribe_windows(audio, asr, turns, opts, progress, on),
        (turns, _) => transcribe_each(audio, asr, turns, opts, progress, on),
    }
}

/// Speaker turns as the engine uses them: "Speaker 1", … by first
/// appearance, a speaker's turns merged across short gaps.
fn prepare_turns(raw: Vec<Turn>) -> Vec<Turn> {
    merge_same_speaker(&label_speakers(raw))
}

/// Without speakers, fixed windows; with [`Strategy::Turns`], each turn alone.
fn transcribe_each(
    audio: &Audio,
    asr: &Asr,
    speaker_turns: Option<Vec<Turn>>,
    opts: &AsrOptions,
    progress: Progress<'_>,
    on: &mut dyn FnMut(Update<'_>),
) -> Result<Transcript> {
    let duration = audio.duration_s();
    let diarized = speaker_turns.is_some();
    let turns = match speaker_turns {
        Some(t) => split_long(&t),
        None => fixed_windows(duration),
    };
    let total = turns.len();
    let mut out = Vec::with_capacity(total);
    let mut lines = 0usize;
    let mut language = opts.language.clone();
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
        let full = asr.transcribe_full(audio.slice(start, end), start, opts)?;
        if language.is_none() {
            language = full.language;
        }
        let segments = full.segments;
        if !diarized {
            // No speakers: keep Whisper's own segments (sentence-like), and
            // drop the overlap of consecutive windows.
            let from = out.last().map(|t: &TranscriptTurn| t.end).unwrap_or(0.0);
            for s in segments.into_iter().filter(|s| !s.text.trim().is_empty() && s.end > from + 0.05) {
                let words: Vec<Word> = s.words.into_iter().filter(|w| w.start >= from).collect();
                let text = if words.is_empty() { s.text.trim().to_string() } else { join_text(words.iter().map(|w| w.word.as_str())) };
                let turn = TranscriptTurn { start: s.start.max(from), end: s.end, speaker: turn.speaker.clone(), text, words, no_speech_prob: Some(s.no_speech_prob) };
                on(Update::Line(&Line { index: lines, start: turn.start, end: turn.end, text: turn.text.clone(), speaker: None }));
                lines += 1;
                on(Update::Turn(&turn));
                out.push(turn);
            }
            continue;
        }
        let text = join_text(segments.iter().map(|s| s.text.as_str()));
        if text.is_empty() {
            continue;
        }
        let words = segments.into_iter().flat_map(|s| s.words).collect();
        let turn = TranscriptTurn { start: turn.start, end: turn.end, speaker: turn.speaker.clone(), text, words, no_speech_prob: None };
        on(Update::Line(&Line { index: lines, start: turn.start, end: turn.end, text: turn.text.clone(), speaker: Some(turn.speaker.clone()) }));
        lines += 1;
        on(Update::Turn(&turn));
        out.push(turn);
    }
    progress(total, total);
    Ok(finish(duration, language, out))
}

fn finish(duration_s: f64, language: Option<String>, turns: Vec<TranscriptTurn>) -> Transcript {
    let mut speakers: Vec<String> = Vec::new();
    for t in &turns {
        if !speakers.contains(&t.speaker) {
            speakers.push(t.speaker.clone());
        }
    }
    Transcript { duration_s, language, speakers, turns, overlaps: Vec::new(), voiceprints: BTreeMap::new() }
}

/// [`Strategy::Windows`] with known turns: pack consecutive speaker turns
/// into windows of at most [`MAX_TURN_S`] (cut between turns, or inside a
/// turn that is longer), transcribe each window, then hand every word to the
/// speaker whose turn overlaps it most, and group consecutive words of one
/// speaker into turns. A turn is final (and sent) once another speaker's
/// word follows it.
fn transcribe_windows(
    audio: &Audio,
    asr: &Asr,
    turns: Vec<Turn>,
    opts: &AsrOptions,
    progress: Progress<'_>,
    on: &mut dyn FnMut(Update<'_>),
) -> Result<Transcript> {
    let duration = audio.duration_s();
    let windows = pack_windows(&split_long(&turns));
    let total = windows.len();
    let mut live = Live::new(Some(turns), false);
    let mut language = opts.language.clone();
    for (i, &(ws, we)) in windows.iter().enumerate() {
        progress(i, total);
        let start = (ws - TURN_PAD_S).max(0.0);
        let end = (we + TURN_PAD_S).min(duration);
        let full = asr.transcribe_full(audio.slice(start, end), start, opts)?;
        if language.is_none() {
            language = full.language;
        }
        full.segments.into_iter().for_each(|s| live.add(s, on));
        live.group(false, on);
    }
    progress(total, total);
    live.group(true, on);
    Ok(finish(duration, language, live.out))
}

/// [`Strategy::Windows`] while the speakers are separated on another thread
/// (see [`transcribe_live`]).
fn transcribe_separating(
    audio: &Audio,
    asr: &Asr,
    separate: Box<dyn FnOnce() -> Result<Vec<Turn>> + Send + '_>,
    opts: &AsrOptions,
    progress: Progress<'_>,
    on: &mut dyn FnMut(Update<'_>),
) -> Result<Transcript> {
    let duration = audio.duration_s();
    let windows = quiet_windows(&audio.samples);
    let total = windows.len();
    let (tx, rx) = std::sync::mpsc::channel();
    // The scope waits for the separation even when the words fail or stop.
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let _ = tx.send(separate().map(prepare_turns));
        });
        let mut live = Live::new(None, true);
        let mut language = opts.language.clone();
        for (i, &(start, end)) in windows.iter().enumerate() {
            progress(i, total);
            if live.turns.is_none()
                && let Ok(r) = rx.try_recv() {
                    live.speakers_known(r?, on);
                }
            let full = asr.transcribe_full(audio.slice(start, end), start, opts)?;
            if language.is_none() {
                language = full.language;
            }
            full.segments.into_iter().for_each(|s| live.add(s, on));
            live.group(false, on);
        }
        progress(total, total);
        if live.turns.is_none() {
            on(Update::WaitingForSpeakers);
            let r = rx.recv().map_err(|_| anyhow::anyhow!("separating speakers stopped"))?;
            live.speakers_known(r?, on);
        }
        live.group(true, on);
        Ok(finish(duration, language, live.out))
    })
}

/// Lines and words of a transcription in windows, grouped into speaker
/// turns once the speakers are known.
struct Live {
    turns: Option<Vec<Turn>>,
    /// Drop words far from any speaker turn (Whisper writing over silence
    /// the speaker separation found no one in): for windows that cover the
    /// whole file rather than only the turns.
    drop_outside: bool,
    lines: Vec<Line>,
    /// Words not yet given to a speaker.
    words: Vec<Word>,
    out: Vec<TranscriptTurn>,
    sent: usize,
}

/// How far from the nearest speaker turn a word may be, with [`Live::drop_outside`].
const OUTSIDE_TURNS_S: f64 = 1.0;

impl Live {
    fn new(turns: Option<Vec<Turn>>, drop_outside: bool) -> Self {
        Live { turns, drop_outside, lines: Vec::new(), words: Vec::new(), out: Vec::new(), sent: 0 }
    }

    fn add(&mut self, seg: AsrSegment, on: &mut dyn FnMut(Update<'_>)) {
        let text = seg.text.trim();
        if !text.is_empty() {
            let line = Line {
                index: self.lines.len(),
                start: seg.start,
                end: seg.end,
                text: text.to_string(),
                speaker: self.turns.as_deref().map(|t| speaker_at(t, seg.start, seg.end)),
            };
            on(Update::Line(&line));
            self.lines.push(line);
        }
        self.words.extend(seg.words);
    }

    fn speakers_known(&mut self, turns: Vec<Turn>, on: &mut dyn FnMut(Update<'_>)) {
        for l in &mut self.lines {
            l.speaker = Some(speaker_at(&turns, l.start, l.end));
        }
        self.turns = Some(turns);
        let speakers: Vec<Option<String>> = self.lines.iter().map(|l| l.speaker.clone()).collect();
        on(Update::Speakers(&speakers));
    }

    /// Give the new words their speakers and send the turns that are final
    /// (all of them when `done`).
    fn group(&mut self, done: bool, on: &mut dyn FnMut(Update<'_>)) {
        let Some(turns) = self.turns.as_deref() else { return };
        for w in std::mem::take(&mut self.words) {
            if self.drop_outside && !turns.iter().any(|t| dist(t, w.start).min(dist(t, w.end)) <= OUTSIDE_TURNS_S) {
                continue;
            }
            let speaker = speaker_at(turns, w.start, w.end);
            match self.out.last_mut() {
                Some(t) if t.speaker == speaker => {
                    t.end = t.end.max(w.end);
                    t.text.push(' ');
                    t.text.push_str(&w.word);
                    t.words.push(w);
                }
                _ => self.out.push(TranscriptTurn { start: w.start, end: w.end, speaker, text: w.word.clone(), words: vec![w], no_speech_prob: None }),
            }
        }
        let upto = if done { self.out.len() } else { self.out.len().saturating_sub(1) };
        for t in &self.out[self.sent.min(upto)..upto] {
            on(Update::Turn(t));
        }
        self.sent = self.sent.max(upto);
    }
}

/// How far back from a window's longest end [`quiet_windows`] looks for a pause.
pub const QUIET_SEARCH_S: f64 = 6.0;

/// Windows of at most [`MAX_TURN_S`] over the whole file, each cut at the
/// quietest 0.3 s in its last [`QUIET_SEARCH_S`] (usually a pause between
/// words or speakers), so no word is cut in two and windows needn't overlap.
pub fn quiet_windows(samples: &[f32]) -> Vec<(f64, f64)> {
    const FRAME: usize = SAMPLE_RATE as usize / 50; // 20 ms
    const SPAN: usize = 15; // frames: 0.3 s
    let energy: Vec<f32> = samples.chunks(FRAME).map(|c| c.iter().map(|x| x * x).sum::<f32>()).collect();
    let duration = samples.len() as f64 / SAMPLE_RATE as f64;
    let frame_s = FRAME as f64 / SAMPLE_RATE as f64;
    let mut windows = Vec::new();
    let mut start = 0.0;
    while duration - start > MAX_TURN_S {
        let from = ((start + MAX_TURN_S - QUIET_SEARCH_S) / frame_s) as usize;
        let to = (((start + MAX_TURN_S) / frame_s) as usize).saturating_sub(SPAN).max(from);
        let mut sum: f32 = energy[from..from + SPAN].iter().sum();
        let mut best = (sum, from);
        for f in from + 1..=to {
            sum += energy[f + SPAN - 1] - energy[f - 1];
            // Ties go to the later span: windows stay long.
            if sum <= best.0 {
                best = (sum, f);
            }
        }
        let cut = (best.1 as f64 + SPAN as f64 / 2.0) * frame_s;
        windows.push((start, cut));
        start = cut;
    }
    if duration > start {
        windows.push((start, duration));
    }
    windows
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
    fn quiet_windows_cut_in_a_pause_and_cover_the_file() {
        // 70 s of "speech" with pauses at 25.0–25.5 s and 50.2–50.8 s.
        let sr = SAMPLE_RATE as usize;
        let mut samples: Vec<f32> = (0..70 * sr).map(|i| ((i % 40) as f32 / 40.0 - 0.5) * 0.4).collect();
        for (a, b) in [(25.0, 25.5), (50.2, 50.8)] {
            samples[(a * sr as f64) as usize..(b * sr as f64) as usize].fill(0.0);
        }
        let w = quiet_windows(&samples);
        assert_eq!(w.len(), 3, "{w:?}");
        assert_eq!(w[0].0, 0.0);
        assert!((25.0..=25.5).contains(&w[0].1), "{w:?}");
        assert!((50.2..=50.8).contains(&w[1].1), "{w:?}");
        assert_eq!(w[2].1, 70.0);
        assert!(w.windows(2).all(|p| p[0].1 == p[1].0), "they don't overlap or leave gaps");
        assert!(w.iter().all(|&(a, b)| b - a <= MAX_TURN_S));
        assert_eq!(quiet_windows(&samples[..10 * sr]), vec![(0.0, 10.0)]);
        assert!(quiet_windows(&[]).is_empty());
    }

    fn seg(start: f64, end: f64, words: &[(&str, f64, f64)]) -> AsrSegment {
        AsrSegment {
            start,
            end,
            text: words.iter().map(|w| w.0).collect::<Vec<_>>().join(" "),
            words: words.iter().map(|&(word, start, end)| Word { word: word.into(), start, end, probability: 0.9 }).collect(),
            no_speech_prob: 0.0,
        }
    }

    #[derive(Debug, PartialEq)]
    enum Seen {
        Line(usize, Option<String>),
        Speakers(Vec<Option<String>>),
        Turn(String, String),
    }

    fn record(seen: &mut Vec<Seen>) -> impl FnMut(Update<'_>) + '_ {
        |u| match u {
            Update::Line(l) => seen.push(Seen::Line(l.index, l.speaker.clone())),
            Update::Speakers(s) => seen.push(Seen::Speakers(s.to_vec())),
            Update::Turn(t) => seen.push(Seen::Turn(t.speaker.clone(), t.text.clone())),
            Update::WaitingForSpeakers => {}
        }
    }

    #[test]
    fn lines_come_first_and_turns_once_final() {
        let mut seen = Vec::new();
        let mut on = record(&mut seen);
        let mut live = Live::new(None, true);
        live.add(seg(0.0, 2.0, &[("Hoi", 0.1, 0.5), ("zäme", 0.6, 1.0), ("Grüezi", 1.6, 1.9)]), &mut on);
        live.group(false, &mut on);
        // Speakers known: A until 1.2 s, B after; nothing said at 9 s.
        live.speakers_known(vec![t(0.0, 1.2, "A"), t(1.4, 5.0, "B")], &mut on);
        live.add(seg(2.0, 4.0, &[("wie", 2.1, 2.4), ("gahts", 2.5, 2.9)]), &mut on);
        live.group(false, &mut on);
        live.add(seg(8.0, 9.5, &[("Untertitel", 9.0, 9.4)]), &mut on);
        live.group(true, &mut on);
        drop(on);
        assert_eq!(
            seen,
            [
                Seen::Line(0, None),
                Seen::Speakers(vec![Some("A".into())]), // A talks most of line 0
                Seen::Line(1, Some("B".into())),
                Seen::Turn("A".into(), "Hoi zäme".into()),
                Seen::Line(2, Some("B".into())),
                // The word over silence is dropped; B's turn ends with the file.
                Seen::Turn("B".into(), "Grüezi wie gahts".into()),
            ]
        );
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

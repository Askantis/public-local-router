//! Whisper through whisper.cpp, with word timestamps from DTW alignment.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use whisper_rs::{
    DtwAhead, DtwMode, DtwModelPreset, FullParams, SamplingStrategy, WhisperContext,
    WhisperContextParameters, WhisperState,
};

/// Which cross-attention heads DTW aligns with.
#[derive(Debug, Clone)]
pub enum Aheads {
    /// The preset for Whisper large-v3-turbo (and fine-tunes that kept its heads).
    LargeV3Turbo,
    /// `(text layer, head)` pairs, e.g. from a fine-tune's `generation_config.json`.
    Custom(Vec<(i32, i32)>),
}

#[derive(Debug, Clone, Default)]
pub struct AsrOptions {
    /// ISO 639-1 code, or `None` to detect. `gsw` (Swiss German) is sent to
    /// Whisper as `de`: the models write Standard German.
    pub language: Option<String>,
    /// Text that guides spelling, such as names.
    pub prompt: Option<String>,
    /// CPU threads for the parts that don't run on the GPU; 0 = automatic.
    pub threads: i32,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Word {
    pub word: String,
    pub start: f64,
    pub end: f64,
    pub probability: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct AsrSegment {
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub words: Vec<Word>,
    pub no_speech_prob: f32,
}

pub struct Asr {
    ctx: WhisperContext,
    // One decoding state, reused for every call: creating one allocates the
    // KV cache and buffers, which cost more than a short turn's decoding.
    state: Mutex<WhisperState>,
}

impl Asr {
    pub fn load(model: &Path, aheads: Aheads) -> Result<Self> {
        let path = model.to_str().ok_or_else(|| anyhow!("model path isn't UTF-8"))?;
        let mut params = WhisperContextParameters::default();
        // DTW needs flash attention off (whisper.cpp silently drops DTW otherwise).
        params.flash_attn(false);
        params.dtw_parameters.mode = match &aheads {
            Aheads::LargeV3Turbo => DtwMode::ModelPreset { model_preset: DtwModelPreset::LargeV3Turbo },
            // whisper.cpp keeps a pointer to the heads for the context's lifetime;
            // a few bytes per loaded model, so leaking them is the simple safe way.
            Aheads::Custom(pairs) => DtwMode::Custom {
                aheads: Box::leak(
                    pairs.iter().map(|&(n_text_layer, n_head)| DtwAhead { n_text_layer, n_head }).collect(),
                ),
            },
        };
        let ctx = WhisperContext::new_with_params(path, params)
            .with_context(|| format!("loading Whisper model {}", model.display()))?;
        let state = Mutex::new(ctx.create_state().context("creating a Whisper state")?);
        Ok(Asr { ctx, state })
    }

    /// Transcribe 16 kHz mono `audio`. Times in the result are offset by
    /// `offset_s`, so a turn's words land on the whole file's timeline.
    pub fn transcribe(&self, audio: &[f32], offset_s: f64, opts: &AsrOptions) -> Result<Vec<AsrSegment>> {
        if audio.len() < 1_600 {
            return Ok(Vec::new()); // < 0.1 s
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        let language = opts.language.as_deref().map(|l| if l == "gsw" { "de" } else { l });
        params.set_language(language);
        params.set_translate(false);
        params.set_no_context(true);
        params.set_token_timestamps(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_temperature(0.0);
        if opts.threads > 0 {
            params.set_n_threads(opts.threads);
        }
        if let Some(p) = &opts.prompt {
            params.set_initial_prompt(p);
        }
        state.full(params, audio).context("running Whisper")?;

        let eot = self.ctx.token_eot();
        let clip_end = audio.len() as f64 / crate::SAMPLE_RATE as f64;
        let mut segments = Vec::new();
        for seg in state.as_iter() {
            let seg_start = seg.start_timestamp() as f64 / 100.0;
            let seg_end = (seg.end_timestamp() as f64 / 100.0).min(clip_end);
            let mut tokens = Vec::new();
            for j in 0..seg.n_tokens() {
                let Some(tok) = seg.get_token(j) else { continue };
                if tok.token_id() >= eot {
                    continue; // timestamps and other special tokens
                }
                let data = tok.token_data();
                let bytes = tok.to_bytes().map(<[u8]>::to_vec).unwrap_or_default();
                if std::env::var_os("LR_DEBUG_TOKENS").is_some() {
                    eprintln!(
                        "TOKEN\t{:.3}\t{}\t{}\t{}\t{}",
                        offset_s,
                        data.t_dtw,
                        data.t0,
                        data.t1,
                        String::from_utf8_lossy(&bytes).replace('\t', " ")
                    );
                }
                tokens.push(TokenPiece {
                    bytes,
                    dtw: (data.t_dtw >= 0).then(|| data.t_dtw as f64 / 100.0),
                    t0: data.t0 as f64 / 100.0,
                    t1: data.t1 as f64 / 100.0,
                    p: tok.token_probability(),
                });
            }
            let words = words_from_tokens(&tokens, seg_start, seg_end, offset_s);
            let text = String::from_utf8_lossy(&tokens.iter().flat_map(|t| t.bytes.clone()).collect::<Vec<_>>())
                .trim()
                .to_string();
            segments.push(AsrSegment {
                start: offset_s + seg_start,
                end: offset_s + seg_end,
                text,
                words,
                no_speech_prob: seg.no_speech_probability(),
            });
        }
        Ok(segments)
    }
}

/// One BPE token: its bytes (a word piece, possibly half a UTF-8 character),
/// its times in the clip, and its probability.
#[derive(Debug, Clone)]
pub(crate) struct TokenPiece {
    pub bytes: Vec<u8>,
    /// Where DTW alignment says the token ends; `None` without DTW.
    pub dtw: Option<f64>,
    /// The decoder's own (coarser) timestamps, used when DTW is missing.
    pub t0: f64,
    pub t1: f64,
    pub p: f32,
}

/// The most a word's first token is assumed to last. A word starts where the
/// previous token ends, but no earlier than this before its first token ends,
/// so a pause before the word isn't counted as part of it. Measured on audio
/// with known word boundaries: 46 ms median start error (bench/RESULTS.md).
const FIRST_TOKEN_MAX_S: f64 = 0.25;

/// Group tokens into words: a token that starts with a space starts a word.
/// Bytes are joined before decoding, so characters split across tokens
/// (ä, ü, …) come out whole.
///
/// Times: whisper.cpp's DTW time marks the *end* of a token. A word ends where
/// its last token ends, and starts where the previous token ended (the segment
/// start for the first word), capped by [`FIRST_TOKEN_MAX_S`]. Without DTW the
/// decoder's `t0`/`t1` are used.
pub(crate) fn words_from_tokens(tokens: &[TokenPiece], seg_start: f64, seg_end: f64, offset: f64) -> Vec<Word> {
    let use_dtw = !tokens.is_empty() && tokens.iter().all(|t| t.dtw.is_some());
    let mut groups: Vec<(usize, usize)> = Vec::new(); // token index ranges
    for (i, tok) in tokens.iter().enumerate() {
        if tok.bytes.first() == Some(&b' ') || groups.is_empty() {
            groups.push((i, i));
        } else {
            groups.last_mut().unwrap().1 = i;
        }
    }
    let clamp = |x: f64| x.clamp(seg_start, seg_end.max(seg_start));
    let mut words: Vec<Word> = Vec::new();
    for &(first, last) in &groups {
        let bytes: Vec<u8> = tokens[first..=last].iter().flat_map(|t| t.bytes.iter().copied()).collect();
        let text = String::from_utf8_lossy(&bytes).trim().to_string();
        if text.is_empty() {
            continue;
        }
        let (start, end) = if use_dtw {
            let end = tokens[last].dtw.unwrap();
            let prev_end = if first > 0 { tokens[first - 1].dtw.unwrap() } else { seg_start };
            (prev_end.max(tokens[first].dtw.unwrap() - FIRST_TOKEN_MAX_S), end)
        } else {
            (tokens[first].t0, tokens[last].t1)
        };
        let (start, end) = (clamp(start), clamp(end));
        let p = tokens[first..=last].iter().map(|t| t.p).sum::<f32>() / (last - first + 1) as f32;
        words.push(Word { word: text, start: offset + start, end: offset + end.max(start), probability: p });
    }
    // DTW times can step backwards by a frame; keep words in order.
    for i in 1..words.len() {
        if words[i].start < words[i - 1].end {
            words[i].start = words[i].start.max(words[i - 1].start);
            words[i - 1].end = words[i - 1].end.min(words[i].start);
        }
        if words[i].end < words[i].start {
            words[i].end = words[i].start;
        }
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(s: &[u8], dtw: f64) -> TokenPiece {
        TokenPiece { bytes: s.to_vec(), dtw: Some(dtw), t0: 0.0, t1: 0.0, p: 0.9 }
    }

    #[test]
    fn groups_tokens_into_words_with_dtw_end_times() {
        // DTW times are token ends.
        let tokens = [tok(b" Hoi", 0.40), tok(b" Th", 0.62), tok(b"omas", 0.80), tok(b",", 0.85), tok(b" wie", 1.10)];
        let words = words_from_tokens(&tokens, 0.0, 1.5, 10.0);
        let got: Vec<(&str, f64, f64)> = words.iter().map(|w| (w.word.as_str(), w.start, w.end)).collect();
        // "Hoi" can't start before 0.40 − 0.25; "Thomas," starts where "Hoi" ended.
        let expect = [("Hoi", 10.15, 10.40), ("Thomas,", 10.40, 10.85), ("wie", 10.85, 11.10)];
        assert_eq!(got.len(), expect.len());
        for ((w, s, e), (ew, es, ee)) in got.iter().zip(expect) {
            assert_eq!(*w, ew);
            assert!((s - es).abs() < 1e-9 && (e - ee).abs() < 1e-9, "{w}: {s}-{e}");
        }
    }

    #[test]
    fn a_pause_before_a_word_isnt_part_of_it() {
        let tokens = [tok(b" eins", 0.40), tok(b" zwei", 2.10)];
        let words = words_from_tokens(&tokens, 0.0, 3.0, 0.0);
        assert!((words[1].start - 1.85).abs() < 1e-9, "starts {}", words[1].start);
    }

    #[test]
    fn joins_bytes_split_across_tokens() {
        // "für" with the ü (0xC3 0xBC) split over two tokens.
        let tokens = [tok(b" f\xC3", 0.1), tok(b"\xBCr", 0.2), tok(b" dich", 0.4)];
        let words = words_from_tokens(&tokens, 0.0, 0.6, 0.0);
        assert_eq!(words[0].word, "für");
        assert_eq!(words[1].word, "dich");
    }

    #[test]
    fn keeps_words_in_order_when_dtw_steps_back() {
        let tokens = [tok(b" eins", 0.50), tok(b" zwei", 0.40), tok(b" drei", 0.90)];
        let words = words_from_tokens(&tokens, 0.0, 1.2, 0.0);
        assert!(words.windows(2).all(|w| w[0].start <= w[1].start && w[0].end <= w[1].start + 1e-9));
        assert!(words.iter().all(|w| w.end >= w.start));
    }

    #[test]
    fn falls_back_to_decoder_timestamps_without_dtw() {
        let t = |s: &[u8], t0: f64, t1: f64| TokenPiece { bytes: s.to_vec(), dtw: None, t0, t1, p: 1.0 };
        let words = words_from_tokens(&[t(b" a", 0.1, 0.3), t(b" b", 0.3, 0.6)], 0.0, 1.0, 0.0);
        assert_eq!((words[0].start, words[0].end, words[1].start, words[1].end), (0.1, 0.3, 0.3, 0.6));
    }
}

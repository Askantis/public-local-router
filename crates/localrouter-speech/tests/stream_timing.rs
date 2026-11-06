//! When lines appear with speakers separated, measured on the real models.
//! Needs the speech models in the standard Hugging Face cache (`lr fetch`)
//! and `bench/out/conv.wav` (`bench/synth.py bench/out`). Writes both
//! transcripts next to it, for `bench/score.py wer`.
//! Run: `cargo test -p localrouter-speech --release -- --ignored --nocapture`.

use std::time::Instant;

use localrouter_speech::{Asr, AsrOptions, Diarizer, Speakers, Strategy, Update, catalog};

fn bench_out() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/out")
}

fn models() -> (Asr, Diarizer) {
    let cache = localrouter_hf::Cache::standard().unwrap();
    let w = catalog::WHISPER_TURBO_Q8;
    let asr = Asr::load(&catalog::whisper_present(&cache, w).unwrap().expect("lr fetch first"), catalog::aheads_for(&w)).unwrap();
    let diarizer = Diarizer::load(&catalog::speaker_models_present(&cache).unwrap().expect("lr fetch first")).unwrap();
    (asr, diarizer)
}

#[derive(Default)]
struct Seen {
    first_line: Option<f64>,
    speakers: Option<f64>,
    waiting: Option<f64>,
    first_turn: Option<f64>,
    lines: usize,
    lines_without_speaker: usize,
    turns: usize,
}

#[test]
#[ignore]
fn lines_come_early_with_speakers() {
    let mut audio = localrouter_audio::decode_file(&bench_out().join("conv.wav")).expect("run bench/synth.py bench/out first");
    // `LR_BENCH_REPEAT=10` for a longer conversation (the file ten times).
    let repeat: usize = std::env::var("LR_BENCH_REPEAT").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    audio.samples = audio.samples.repeat(repeat);
    let rounds = if repeat > 1 { 1 } else { 3 };
    let (asr, mut diarizer) = models();
    let opts = AsrOptions { language: Some("de".into()), ..Default::default() };
    // Warm both up (Metal shaders, first allocations).
    let _ = asr.transcribe(audio.slice(0.0, 5.0), 0.0, &opts).unwrap();
    let _ = diarizer.run(audio.slice(0.0, 10.0)).unwrap();

    for round in 0..rounds {
        // Before: speakers first, then the words in windows of their turns.
        let t0 = Instant::now();
        let mut before = Seen::default();
        let turns = localrouter_speech::merge_same_speaker(&localrouter_speech::label_speakers(diarizer.run(&audio.samples).unwrap()));
        let sequential = localrouter_speech::transcribe_live(&audio, &asr, Speakers::Known(turns), &opts, Strategy::Windows, &mut |_, _| {}, &mut |u| note(&mut before, t0, u)).unwrap();
        let before_total = t0.elapsed().as_secs_f64();

        // Now: both at once.
        let t0 = Instant::now();
        let mut now = Seen::default();
        let d = &mut diarizer;
        let samples = &audio.samples;
        let live = localrouter_speech::transcribe_live(&audio, &asr, Speakers::Separate(Box::new(move || d.run(samples))), &opts, Strategy::Windows, &mut |_, _| {}, &mut |u| note(&mut now, t0, u)).unwrap();
        let now_total = t0.elapsed().as_secs_f64();

        println!(
            "{:.0} s, round {round}: before {before_total:.2} s in all, first line {:.2} s, first turn {:.2} s | now {now_total:.2} s in all, first line {:.2} s, words done {:.2} s, speakers {:.2} s, first turn {:.2} s; {} lines, {} without a speaker at first",
            audio.duration_s(),
            before.first_line.unwrap(),
            before.first_turn.unwrap(),
            now.first_line.unwrap(),
            now.waiting.unwrap_or(f64::NAN),
            now.speakers.unwrap(),
            now.first_turn.unwrap(),
            now.lines,
            now.lines_without_speaker,
        );
        assert!(now.first_line.unwrap() < before.first_line.unwrap(), "lines come before the speakers are separated");
        assert_eq!(now.turns, live.turns.len(), "every turn is sent once");
        assert_eq!(before.turns, sequential.turns.len());
        assert!(live.speakers.len() >= 2, "{:?}", live.speakers);
        if round == 0 && repeat == 1 {
            std::fs::write(bench_out().join("lr-conv-sequential.json"), serde_json::to_string_pretty(&sequential).unwrap()).unwrap();
            std::fs::write(bench_out().join("lr-conv-live.json"), serde_json::to_string_pretty(&live).unwrap()).unwrap();
        }
    }
}

fn note(s: &mut Seen, t0: Instant, u: Update<'_>) {
    let at = t0.elapsed().as_secs_f64();
    match u {
        Update::Line(l) => {
            s.first_line.get_or_insert(at);
            s.lines += 1;
            s.lines_without_speaker += l.speaker.is_none() as usize;
        }
        Update::Speakers(_) => {
            s.speakers.get_or_insert(at);
        }
        Update::Turn(_) => {
            s.first_turn.get_or_insert(at);
            s.turns += 1;
        }
        Update::WaitingForSpeakers => {
            s.waiting.get_or_insert(at);
        }
    }
}

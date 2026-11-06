//! How alike voiceprints of the same and of different people are, measured on
//! the real models, to choose the threshold an app recognises people at.
//! Needs the speaker models in the standard Hugging Face cache (`lr fetch`),
//! `bench/out/conv.wav` (`bench/synth.py bench/out`) and `bench/out/voices`
//! (`bench/voices.py bench/out/voices`); `LR_REAL_SAMPLE=<dir>` adds a real
//! two-person recording (`sample.wav` + `sample.rttm`, e.g. pyannote's sample).
//! Run: `cargo test -p localrouter-speech --release --test voiceprints -- --ignored --nocapture`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use localrouter_speech::{Diarizer, Turn, Voiceprint, catalog};

fn bench_out() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/out")
}

fn diarizer() -> Diarizer {
    let cache = localrouter_hf::Cache::standard().unwrap();
    Diarizer::load(&catalog::speaker_models_present(&cache).unwrap().expect("lr fetch first")).unwrap()
}

fn audio(path: &Path) -> Vec<f32> {
    localrouter_audio::decode_file(path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display())).samples
}

fn truth(path: &Path) -> Vec<Turn> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    v.as_array()
        .unwrap()
        .iter()
        .map(|t| Turn { start: t["start"].as_f64().unwrap(), end: t["end"].as_f64().unwrap(), speaker: t["speaker"].as_str().unwrap().into() })
        .collect()
}

fn rttm(path: &Path) -> Vec<Turn> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (start, dur): (f64, f64) = (f.get(3)?.parse().ok()?, f.get(4)?.parse().ok()?);
            Some(Turn { start, end: start + dur, speaker: f.get(7)?.to_string() })
        })
        .collect()
}

fn sim(a: &Voiceprint, b: &Voiceprint) -> f32 {
    a.similarity(b).unwrap()
}

/// The diarized speakers named by the true speaker they overlap most.
fn named_like_truth(d: &mut Diarizer, samples: &[f32], truth: &[Turn]) -> BTreeMap<String, Voiceprint> {
    let turns = d.run(samples).unwrap();
    let labelled = localrouter_speech::label_speakers(turns);
    let mut out = BTreeMap::new();
    for (label, vp) in d.voiceprints() {
        let mut overlap: BTreeMap<&str, f64> = BTreeMap::new();
        for t in labelled.iter().filter(|t| &t.speaker == label) {
            for u in truth {
                *overlap.entry(&u.speaker).or_default() += (t.end.min(u.end) - t.start.max(u.start)).max(0.0);
            }
        }
        let total: f64 = overlap.values().sum();
        let (who, most) = overlap.into_iter().max_by(|a, b| a.1.total_cmp(&b.1)).map(|(w, o)| (w.to_string(), o)).unwrap();
        let purity = most / total.max(1e-9);
        println!("  {label} = {who} ({:.0} % of its speech): {:.1} s", purity * 100.0, vp.speech_s);
        // A speaker the separation mixed with another isn't one person's voice.
        if purity >= 0.9 {
            out.insert(who, vp.clone());
        }
    }
    out
}

fn stats(name: &str, xs: &mut [f32]) {
    xs.sort_by(|a, b| a.total_cmp(b));
    let n = xs.len();
    if n == 0 {
        return;
    }
    let p = |q: f64| xs[((n - 1) as f64 * q).round() as usize];
    let mean = xs.iter().sum::<f32>() / n as f32;
    println!("{name:<44} n={n:<4} min {:.3}  p5 {:.3}  median {:.3}  mean {mean:.3}  p95 {:.3}  max {:.3}", xs[0], p(0.05), p(0.5), p(0.95), xs[n - 1]);
}

#[test]
#[ignore]
fn same_and_different_people() {
    let mut d = diarizer();
    let voices_dir = bench_out().join("voices");
    let stems: Vec<String> = serde_json::from_slice(&std::fs::read(voices_dir.join("voices.json")).expect("bench/voices.py bench/out/voices first")).unwrap();

    // 1. Each voice reading two texts (some in two languages), whole clip.
    let mut clips: Vec<(String, char, Voiceprint)> = Vec::new();
    for stem in &stems {
        for part in ['a', 'b'] {
            let samples = audio(&voices_dir.join(format!("{stem}_{part}.wav")));
            let dur = samples.len() as f64 / 16_000.0;
            let vp = d.voiceprints_of(&samples, &[Turn { start: 0.4, end: dur - 0.4, speaker: stem.clone() }]).unwrap().remove(stem).unwrap();
            clips.push((stem.clone(), part, vp));
        }
    }
    let (mut same, mut diff) = (Vec::new(), Vec::new());
    let mut worst_same = (1.0f32, String::new());
    let mut worst_diff = (-1.0f32, String::new());
    for (i, a) in clips.iter().enumerate() {
        for b in &clips[i + 1..] {
            let s = sim(&a.2, &b.2);
            if a.0 == b.0 {
                same.push(s);
                if s < worst_same.0 {
                    worst_same = (s, a.0.clone());
                }
            } else if a.1 != b.1 {
                // Different texts, as in two recordings.
                diff.push(s);
                if s > worst_diff.0 {
                    worst_diff = (s, format!("{}_{} / {}_{}", a.0, a.1, b.0, b.1));
                }
            }
        }
    }
    println!("\nclips ({} voices, ~20 s each):", stems.len());
    stats("same voice, other text (some other language)", &mut same);
    stats("different voices, other text", &mut diff);
    println!("  least alike same voice: {worst_same:?}; most alike different voices: {worst_diff:?}");

    // 2. Meetings, voiceprints from the speaker separation (as after a transcription).
    let mut meetings: Vec<BTreeMap<String, Voiceprint>> = Vec::new();
    let mut by_turns: Vec<BTreeMap<String, Voiceprint>> = Vec::new();
    for name in ["meet1", "meet2"] {
        println!("{name}:");
        let samples = audio(&voices_dir.join(format!("{name}.wav")));
        let t = truth(&voices_dir.join(format!("{name}.json")));
        let from_diar = named_like_truth(&mut d, &samples, &t);
        // The same speakers from their true turns (as for an older project).
        let from_turns = d.voiceprints_of(&samples, &t).unwrap();
        for (who, vp) in &from_diar {
            println!("  {who}: separation vs turns {:.3}", sim(vp, &from_turns[who]));
        }
        meetings.push(from_diar);
        by_turns.push(from_turns);
    }
    println!("conv (bench/synth.py: anna, grandpa, grandma):");
    let conv_truth: Vec<Turn> = truth(&bench_out().join("conv.json"))
        .into_iter()
        .map(|t| Turn { speaker: ["anna", "grandpa", "grandma"][t.speaker[1..].parse::<usize>().unwrap() - 1].into(), ..t })
        .collect();
    let conv_audio = audio(&bench_out().join("conv.wav"));
    meetings.push(named_like_truth(&mut d, &conv_audio, &conv_truth));
    by_turns.push(d.voiceprints_of(&conv_audio, &conv_truth).unwrap());

    let (mut m_same, mut m_diff) = (Vec::new(), Vec::new());
    for (i, a) in meetings.iter().enumerate() {
        for b in &meetings[i + 1..] {
            for (wa, va) in a {
                for (wb, vb) in b {
                    let s = sim(va, vb);
                    println!("  {wa} ~ {wb}: {s:.3}");
                    if wa == wb { m_same.push(s) } else { m_diff.push(s) }
                }
            }
        }
    }
    println!();
    stats("meetings: same person, other meeting", &mut m_same);
    stats("meetings: different people, other meeting", &mut m_diff);

    // 3. A meeting's speaker against the clips (remembered from a clip, heard in a meeting).
    let (mut x_same, mut x_diff) = (Vec::new(), Vec::new());
    for m in &meetings {
        for (who, vp) in m {
            for (stem, _, c) in &clips {
                let s = sim(vp, c);
                if stem == who { x_same.push(s) } else { x_diff.push(s) }
            }
        }
    }
    stats("meeting vs clip: same person", &mut x_same);
    stats("meeting vs clip: different people", &mut x_diff);

    // 3b. The same from the meetings' true turns (an older project; no separation errors).
    let (mut t_same, mut t_diff, mut tm_same, mut tm_diff) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (i, m) in by_turns.iter().enumerate() {
        for (who, vp) in m {
            for (stem, _, c) in &clips {
                let s = sim(vp, c);
                if stem == who { t_same.push(s) } else { t_diff.push(s) }
            }
            for other in &by_turns[i + 1..] {
                for (w2, v2) in other {
                    if w2 == who { tm_same.push(sim(vp, v2)) } else { tm_diff.push(sim(vp, v2)) }
                }
            }
        }
    }
    stats("true turns: meeting vs clip, same person", &mut t_same);
    stats("true turns: meeting vs clip, different", &mut t_diff);
    stats("true turns: meeting vs meeting, same person", &mut tm_same);
    stats("true turns: meeting vs meeting, different", &mut tm_diff);

    // 4. Real people (optional): each speaker's first half against their second half.
    if let Some(dir) = std::env::var_os("LR_REAL_SAMPLE").map(PathBuf::from) {
        let samples = audio(&dir.join("sample.wav"));
        let turns = rttm(&dir.join("sample.rttm"));
        let mut halves: Vec<(String, Voiceprint)> = Vec::new();
        let speakers: Vec<String> = turns.iter().map(|t| t.speaker.clone()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        for s in &speakers {
            let mine: Vec<&Turn> = turns.iter().filter(|t| &t.speaker == s).collect();
            let total: f64 = mine.iter().map(|t| t.end - t.start).sum();
            let mut acc = 0.0;
            let (mut first, mut second) = (Vec::new(), Vec::new());
            for t in mine {
                if acc < total / 2.0 { first.push(t.clone()) } else { second.push(t.clone()) }
                acc += t.end - t.start;
            }
            for (half, ts) in [("1", first), ("2", second)] {
                // Other speakers' turns stay in, so overlaps are cut out.
                let mut all: Vec<Turn> = ts.iter().map(|t| Turn { speaker: format!("{s}#{half}"), ..t.clone() }).collect();
                all.extend(turns.iter().filter(|t| &t.speaker != s).cloned());
                if let Some(vp) = d.voiceprints_of(&samples, &all).unwrap().remove(&format!("{s}#{half}")) {
                    println!("real {s} half {half}: {:.1} s", vp.speech_s);
                    halves.push((s.clone(), vp));
                }
            }
        }
        let (mut r_same, mut r_diff) = (Vec::new(), Vec::new());
        for (i, a) in halves.iter().enumerate() {
            for b in &halves[i + 1..] {
                if a.0 == b.0 { r_same.push(sim(&a.1, &b.1)) } else { r_diff.push(sim(&a.1, &b.1)) }
            }
        }
        stats("real: same person, other half", &mut r_same);
        stats("real: different people", &mut r_diff);
        let mut r_vs_synth: Vec<f32> = halves.iter().flat_map(|(_, a)| clips.iter().map(move |(_, _, c)| sim(a, c))).collect();
        stats("real vs synthetic voices", &mut r_vs_synth);
    }
}

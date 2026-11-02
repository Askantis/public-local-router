# Speech-engine spike: results

2 November 2026, on an Apple silicon desktop Mac. The Rust engine is `lr` from this repository (whisper.cpp v1.8.3 through whisper-rs 0.16 with Metal, speakrs 0.7). The reference is Scribely beta's Python pipeline (transformers Whisper on MPS, pyannote community-1) on the same files. Reproduce with `bench/run.sh` (see [README.md](README.md)).

## Verdict

**Go for the engine as built:** Whisper large-v3-turbo in ggml with DTW word timing, speaker separation with speakrs on the CPU, and windows that span speaker changes. On every check that could be run, it matches or beats Scribely beta. It uses a sixth of the memory and is about nine times faster.

**Not yet decided:**
- The Swiss-German word error rate on the FHNW test set: its data drive wasn't connected.
- Real multi-speaker recordings.
- The 8 GB Mac.
- A notarized build.

Those are listed at the end.

## Gates

| Gate (plan) | Target | Rust engine | Scribely beta | |
|---|---|---|---|---|
| Word start time vs. ground truth | median ≤ 60 ms, p90 ≤ 200 ms | **46 ms**, 161 ms | 201 ms, 321 ms | ✅ |
| Word end time vs. ground truth | — | 76 ms median | 89 ms | ✅ |
| WER, 3-speaker meeting (German, 2 files) | ≤ beta + 0.7 pt | **0.0 %**, **0.0 %** | 7.4 %, 1.3 % | ✅ |
| WER, Swiss German (FHNW test split) | ≤ beta + 0.3 / 0.7 pt | — | — | ⏳ data drive not connected |
| Diarization DER, easy meeting (collar 0.25 s) | ≤ community-1 + 1.0 pt | 0.9 % | 0.9 % | ✅ identical |
| Diarization DER, hard meeting (two similar voices) | ≤ community-1 + 1.0 pt | 35.1 % | 33.4 % | ⚠️ +1.7 pt, both merge the same two voices |
| Speaking time given to the right speaker | — | 99.3 % / 65.2 % | 99.5 % / 66.5 % | ✅ on par |
| End-to-end speed, 64 s meeting | ≥ 2× beta | **4.2 s** (16.6× real time) | 39 s | ✅ ~9× faster |
| Peak memory (turbo q8_0 + speakers) | ≤ 2.5 GB | **1.41 GB** | 8.7 GB | ✅ |
| Model load, warm | ≤ 3 s | 0.4 s | — | ✅ |
| First load after install | ≤ 3 s | 6–13 s (one-time Metal shader compile) | — | ⚠️ warm up during onboarding |
| Live 28 s window on an 8 GB Mac | ≤ 800 ms p95 | — | — | ⏳ needs that Mac |
| Formats | m4a, mp3, wav, flac, Opus, mov | WAV, AIFF, FLAC, AAC/ALAC in M4A and MP4 ✅; **Opus ✗** (no decoder yet); MP3/MOV not tested | — | ⚠️ add the libopus adapter |
| Grüezi in ggml | converts, aligns | f16 converted; WER 0.0 % on the meeting, word start 67 ms median / 181 ms p90 with its own alignment heads | — | ✅ q8_0/q5_0 still to make |
| Notarized build runs Metal on a clean Mac, app ≤ 80 MB | | — | — | ⏳ Phase 2/3 |

The test audio comes from macOS voices with exactly known word times and turns. It is clean speech, so these numbers show the engine works and is on par; they don't replace real recordings.

## What the spike found and changed

1. **whisper.cpp's DTW times mark the end of a token, not its start.** Using the first token's time as a word's start put every word 250 ms late. The engine now:
   - ends a word where its last token ends;
   - starts it where the previous token ended, but no earlier than 0.25 s before its first token ends.

   That rule took the median start error from 243 ms to 46 ms. Scribely beta's transformers alignment is 200 ms early on the same audio.
2. **whisper.cpp aborted the whole process** on a segment shorter than the DTW median filter (`WHISPER_ASSERT(filter_width < a->ne[2])`). A 0.7 s speaker turn was enough. Fixed in [`third_party/whisper-rs-sys`](../third_party/README.md): such segments skip DTW and use the decoder's timestamps. This needs reporting upstream.
3. **speakrs' Core ML backend mixes up similar voices.** It gave 24 % confusion on a meeting where pyannote and speakrs' own CPU backend give 0.5 %. The engine uses the CPU backend by default: it matches pyannote exactly, at 29× real time. Core ML (135× real time) stays available with `LR_DIARIZE_MODE=coreml` for when it's fixed upstream.
4. **Transcribing each speaker turn on its own, as the beta does, makes Whisper invent text for very short turns.** Four turns of 0.3–0.6 s came back as whole made-up sentences. The engine now transcribes windows of up to 28 s across speaker changes and gives each word to the speaker talking at that moment. WER went from 10.7 % to 0.0 % on the hard meeting, and the engine got twice as fast. The beta's per-turn behaviour remains available as `lr transcribe --per-turn`.
5. **The Hugging Face cache is shared for real.** Python's `huggingface_hub` finds the files `lr fetch` downloaded (`try_to_load_from_cache` and `scan_cache_dir`). Downloads resume after an interruption and are checked against their SHA-256.

## Still open before Phase 2 and 3 rely on the engine

1. **WER on the FHNW Swiss-German test split.** The data sits on a drive that wasn't connected. Then compare multilingual turbo q8_0 and f16, and Grüezi f16 and q8_0, against Scribely beta's recorded predictions (`ml-scribely-training/eval_*_test.records.jsonl`).
2. **At least 5 h of real multi-speaker Swiss-German recordings with corrected speaker turns (RTTM)**, for DER and cpWER. These don't exist yet.
3. **The 8 GB Mac**: live-window latency and memory.
4. **Opus support** (`symphonia-adapter-libopus`), and MP3/MOV tests.
5. **Grüezi q8_0 and q5_0** (whisper.cpp's `quantize`), and hosting the ggml files in the private `askantis/gruezi-v1` repository with the per-file mirror.
6. **Speaker-count hints** (`num/min/max speakers`). speakrs has none; the beta passes them to pyannote.

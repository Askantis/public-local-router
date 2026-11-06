# Speech-engine benchmark

Checks the Rust speech engine (`crates/localrouter-speech`, run through `lr`) against ground truth, and against Scribely beta's Python pipeline on the same audio. Results and the go/no-go call are in [RESULTS.md](RESULTS.md).

```bash
cargo build --release -p lr
target/release/lr fetch                      # Whisper + speaker models into the Hugging Face cache
bench/run.sh                                 # synthesize audio, run lr, score
BETA_DIR=~/path/to/app-scribely-desktop bench/run.sh   # also score Scribely beta (needs its venv)
```

| File | What it does |
|---|---|
| `synth.py` | Makes audio with exactly known timing from macOS voices: `words.wav` (31 words placed at known times), `conv.wav` (a 15-turn meeting between three voices with known turns) |
| `voices.py` | Makes audio for voiceprints (recognising people across recordings): 18 voices reading two texts each (some in two languages) and two meetings with known turns, into `out/voices`; `crates/localrouter-speech/tests/voiceprints.rs` measures how alike same and different voices are |
| `score.py` | Word start/end error, diarization error rate (DER, frame-based, optimal speaker mapping), word error rate and speaker attribution |
| `run.sh` | Runs everything; output goes to `bench/out/` (git-ignored) |

`LR_DIARIZE_MODE=coreml` (or `coreml-fast`) switches speaker separation to speakrs' Core ML backend, and `LR_DEBUG_TOKENS=1` prints every Whisper token with its DTW and decoder times, which is how the word-timing rule was measured.

Synthetic voices are clean and too alike to stand in for real recordings. The gates in the plan also need the FHNW Swiss-German test set and real multi-speaker recordings with corrected speaker turns; see RESULTS.md for what is still open.

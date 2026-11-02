# Patched dependencies

| Folder | Upstream | Why it's here |
|---|---|---|
| `whisper-rs-sys` | crates.io `whisper-rs-sys` 0.15.0 (bundles whisper.cpp v1.8.3), MIT/Unlicense | One fix in `whisper.cpp/src/whisper.cpp`, `whisper_exp_compute_token_level_timestamps_dtw`: segments shorter than the DTW median filter are skipped instead of aborting the process (`WHISPER_ASSERT(filter_width < a->ne[2])`). Search for "Local Router patch". |

Used through `[patch.crates-io]` in the workspace `Cargo.toml`. To be replaced by a fork in the Askantis organisation once the fix is upstream or the fork exists.

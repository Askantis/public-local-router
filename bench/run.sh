#!/bin/sh
# Run the speech-engine benchmark: synthesize the audio, run `lr`, score it.
# Set BETA_DIR to a Scribely beta checkout (with its venv) to score the beta too.
#   bench/run.sh [OUT_DIR]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=${1:-"$here/out"}
lr="$here/../target/release/lr"
mkdir -p "$out"
[ -f "$out/conv.wav" ] || python3 "$here/synth.py" "$out"
[ -x "$lr" ] || (cd "$here/.." && cargo build --release -p lr)

echo "== Rust engine (lr)"
"$lr" transcribe "$out/words.wav" --language de --no-diarize --json "$out/lr-words.json" 2>&1 | tr '\r' '\n' | grep "real time"
python3 "$here/score.py" words "$out/words.json" "$out/lr-words.json"
"$lr" diarize "$out/conv.wav" --json "$out/lr-conv-diar.json" 2>&1 | tail -1
python3 "$here/score.py" der "$out/conv.json" "$out/lr-conv-diar.json"
/usr/bin/time -l "$lr" transcribe "$out/conv.wav" --language de --json "$out/lr-conv.json" 2>&1 | grep -E "real time|peak memory"
python3 "$here/score.py" wer "$out/conv.txt" "$out/lr-conv.json" --turns "$out/conv.json"

if [ -n "${BETA_DIR:-}" ]; then
  echo "== Scribely beta (Python)"
  for f in words conv; do
    mkdir -p "$out/beta-$f"
    (cd "$BETA_DIR" && /usr/bin/time -l venv/bin/python transcribe.py "$out/$f.wav" --language de --format json --output-dir "$out/beta-$f") 2>&1 | grep -E "peak memory|^ +[0-9.]+ real" || true
  done
  python3 "$here/score.py" words "$out/words.json" "$out"/beta-words/*_transkribiert.json
  python3 "$here/score.py" der "$out/conv.json" "$out"/beta-conv/*_transkribiert.json
  python3 "$here/score.py" wer "$out/conv.txt" "$out"/beta-conv/*_transkribiert.json --turns "$out/conv.json"
fi

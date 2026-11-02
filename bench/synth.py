#!/usr/bin/env python3
"""Make benchmark audio with exactly known timing, using macOS `say` voices.

    python3 bench/synth.py OUT_DIR

Writes, into OUT_DIR:
  words.wav + words.json   31 German words, each spoken separately and placed at
                           a known time: ground truth for word timestamps.
  conv.wav + conv.json     A 15-turn meeting between three voices with known
                           turns: ground truth for speaker diarization.
  conv.txt                 The meeting's text, for word error rate.
Standard library only (needs macOS for `say` and `afconvert`).
"""
import array, json, random, subprocess, sys, tempfile, wave
from pathlib import Path

SR = 16000
WORDS = ("Guten Morgen zusammen heute besprechen wir die Planung für den nächsten Monat "
         "Zuerst schauen wir uns die Zahlen an danach entscheiden wir gemeinsam wie es "
         "weitergeht Vielen Dank für eure Geduld").split()
LONG_GAP_AFTER = {"Monat", "an", "weitergeht"}
VOICES = ["Anna", "Grandpa (German (Germany))", "Grandma (German (Germany))"]
TURNS = [
    (0, "Guten Morgen zusammen, schön dass ihr alle da seid."),
    (1, "Morgen Anna. Ich habe die Zahlen vom letzten Quartal mitgebracht."),
    (2, "Sehr gut, dann können wir gleich mit dem Budget anfangen."),
    (0, "Einverstanden. Wie sieht es mit den Kosten für das neue Büro aus?"),
    (1, "Die Kosten sind etwas höher als geplant, vor allem wegen der Möbel."),
    (2, "Das habe ich befürchtet. Können wir bei den Möbeln noch sparen?"),
    (1, "Vielleicht, wenn wir einige Tische gebraucht kaufen."),
    (0, "Das finde ich eine gute Idee. Wer kümmert sich darum?"),
    (2, "Ich kann das übernehmen und bis Freitag Angebote einholen."),
    (0, "Perfekt. Dann sprechen wir als nächstes über die Planung für den Umzug."),
    (1, "Der Umzug ist für Ende Monat geplant. Die Firma hat schon zugesagt."),
    (2, "Und was machen wir mit den alten Computern?"),
    (0, "Die alten Computer spenden wir an eine Schule in der Nähe."),
    (1, "Gute Lösung. Dann haben wir alles besprochen, oder?"),
    (2, "Ja, ich glaube schon. Vielen Dank euch beiden."),
]


def speak(voice: str, text: str, tmp: Path) -> list[float]:
    aiff, wav = tmp / "s.aiff", tmp / "s.wav"
    # `say` occasionally hangs when the speech service stalls; retry instead of waiting forever.
    for attempt in range(3):
        try:
            subprocess.run(["say", "-v", voice, "-o", str(aiff), text], check=True, timeout=60)
            break
        except subprocess.TimeoutExpired:
            if attempt == 2:
                raise
    subprocess.run(["afconvert", "-f", "WAVE", "-d", f"LEI16@{SR}", "-c", "1", str(aiff), str(wav)], check=True)
    with wave.open(str(wav)) as w:
        pcm = array.array("h", w.readframes(w.getnframes()))
    samples = [x / 32768 for x in pcm]
    peak = max(abs(x) for x in samples) or 1.0
    loud = [i for i, x in enumerate(samples) if abs(x) > 0.02 * peak]
    return samples[max(loud[0] - 80, 0): loud[-1] + 80]  # trim TTS silence, keep 5 ms


def write_wav(path: Path, samples: list[float]) -> None:
    pcm = array.array("h", (max(-32768, min(32767, int(x * 32767))) for x in samples))
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1); w.setsampwidth(2); w.setframerate(SR); w.writeframes(pcm.tobytes())


def main() -> None:
    out = Path(sys.argv[1]); out.mkdir(parents=True, exist_ok=True)
    rng = random.Random(7)
    with tempfile.TemporaryDirectory() as t:
        tmp = Path(t)
        audio, truth, pos = [0.0] * int(0.5 * SR), [], 0.5
        for word in WORDS:
            s = speak("Anna", word, tmp)
            truth.append({"word": word, "start": round(pos + 0.005, 3), "end": round(pos + len(s) / SR - 0.005, 3)})
            gap = 0.45 if word in LONG_GAP_AFTER else 0.12
            audio += s + [0.0] * int(gap * SR); pos += len(s) / SR + gap
        write_wav(out / "words.wav", audio)
        (out / "words.json").write_text(json.dumps(truth, ensure_ascii=False, indent=1))

        audio, truth, pos = [0.0] * int(0.5 * SR), [], 0.5
        for speaker, text in TURNS:
            s = speak(VOICES[speaker], text, tmp)
            truth.append({"start": round(pos, 3), "end": round(pos + len(s) / SR, 3), "speaker": f"S{speaker + 1}"})
            gap = rng.uniform(0.25, 0.9)
            audio += s + [0.0] * int(gap * SR); pos += len(s) / SR + gap
        audio = [x + rng.gauss(0, 0.002) for x in audio]  # a little room noise
        write_wav(out / "conv.wav", audio)
        (out / "conv.json").write_text(json.dumps(truth, indent=1))
        (out / "conv.txt").write_text("\n".join(t for _, t in TURNS) + "\n")
    print(f"wrote {out}/words.wav ({len(WORDS)} words) and {out}/conv.wav ({len(TURNS)} turns)")


if __name__ == "__main__":
    main()

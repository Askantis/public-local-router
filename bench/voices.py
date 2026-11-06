#!/usr/bin/env python3
"""Make audio for measuring voiceprints (speaker recognition across recordings),
using macOS `say` voices.

    python3 bench/voices.py OUT_DIR

Writes, into OUT_DIR:
  <voice>_a.wav, <voice>_b.wav   Each voice reading two different texts (~20 s).
                                 The voices that speak German and English read
                                 text a in German and text b in English.
  meet1.wav + meet1.json         A meeting of three voices with known turns,
  meet2.wav + meet2.json         and another one: two of the same voices, one new.
  voices.json                    The voices, by file stem.
Synthetic voices are clean and some are alike; real recordings are still the
better test. Standard library only (macOS `say` and `afconvert`).
"""
import json, random, sys, tempfile
from pathlib import Path

from synth import speak, write_wav, SR

DE_A = ("Guten Morgen. Heute möchte ich kurz erklären, wie wir das Projekt im nächsten Quartal planen. "
        "Zuerst schauen wir uns das Budget an, danach die Termine, und am Schluss besprechen wir, "
        "wer welche Aufgaben übernimmt. Wenn etwas unklar ist, fragt bitte sofort nach.")
DE_B = ("Am Wochenende war ich mit der Familie in den Bergen. Das Wetter war wunderbar, "
        "wir sind früh losgelaufen und waren am Mittag oben beim See. Auf dem Rückweg haben wir "
        "in einer kleinen Hütte Käse gekauft, und abends waren alle sehr müde.")
EN_B = ("Last weekend I went to the mountains with my family. The weather was wonderful, "
        "we started early and reached the lake by noon. On the way back we bought some cheese "
        "at a small hut, and in the evening everyone was very tired.")
EN_A = ("Good morning. Today I would like to explain how we plan the project for the next quarter. "
        "First we look at the budget, then at the dates, and at the end we discuss who takes "
        "which tasks. If anything is unclear, please ask right away.")

# (file stem, voice for text a, voice for text b, text a, text b)
VOICES = [
    ("anna", "Anna", "Anna", DE_A, DE_B),
    ("daniel", "Daniel", "Daniel", EN_A, EN_B),
    ("karen", "Karen", "Karen", EN_A, EN_B),
    ("moira", "Moira", "Moira", EN_A, EN_B),
    ("samantha", "Samantha", "Samantha", EN_A, EN_B),
    ("tessa", "Tessa", "Tessa", EN_A, EN_B),
    ("rishi", "Rishi", "Rishi", EN_A, EN_B),
    ("fred", "Fred", "Fred", EN_A, EN_B),
    ("kathy", "Kathy", "Kathy", EN_A, EN_B),
    ("ralph", "Ralph", "Ralph", EN_A, EN_B),
]
for name in ["Eddy", "Flo", "Grandma", "Grandpa", "Reed", "Rocko", "Sandy", "Shelley"]:
    VOICES.append((name.lower(), f"{name} (German (Germany))", f"{name} (English (US))", DE_A, EN_B))

MEET1 = [("anna", "Anna", "Guten Morgen zusammen, schön dass ihr alle da seid."),
         ("grandpa", "Grandpa (German (Germany))", "Morgen Anna. Ich habe die Zahlen vom letzten Quartal mitgebracht."),
         ("sandy", "Sandy (German (Germany))", "Sehr gut, dann können wir gleich mit dem Budget anfangen."),
         ("anna", "Anna", "Einverstanden. Wie sieht es mit den Kosten für das neue Büro aus?"),
         ("grandpa", "Grandpa (German (Germany))", "Die Kosten sind etwas höher als geplant, vor allem wegen der Möbel."),
         ("sandy", "Sandy (German (Germany))", "Das habe ich befürchtet. Können wir bei den Möbeln noch sparen?"),
         ("anna", "Anna", "Das finde ich eine gute Idee. Wer kümmert sich darum?"),
         ("grandpa", "Grandpa (German (Germany))", "Ich kann das übernehmen und bis Freitag Angebote einholen."),
         ("sandy", "Sandy (German (Germany))", "Perfekt. Dann sprechen wir als nächstes über den Umzug.")]
MEET2 = [("sandy", "Sandy (German (Germany))", "Hallo miteinander, wir fangen gleich an mit dem Rückblick."),
         ("eddy", "Eddy (German (Germany))", "Danke. Der Umzug hat gut geklappt, nur das Internet fehlte zwei Tage."),
         ("anna", "Anna", "Das war ärgerlich, aber jetzt läuft alles wieder."),
         ("sandy", "Sandy (German (Germany))", "Wie geht es mit den neuen Tischen, sind die schon geliefert?"),
         ("eddy", "Eddy (German (Germany))", "Die Hälfte ist da, der Rest kommt nächste Woche am Dienstag."),
         ("anna", "Anna", "Gut. Dann machen wir am Mittwoch ein kleines Fest im neuen Büro."),
         ("sandy", "Sandy (German (Germany))", "Gerne, ich bringe einen Kuchen mit."),
         ("eddy", "Eddy (German (Germany))", "Und ich kümmere mich um die Getränke, das wird schön.")]


def meeting(out: Path, name: str, turns, tmp: Path, rng: random.Random) -> None:
    audio, truth, pos = [0.0] * int(0.5 * SR), [], 0.5
    for who, voice, text in turns:
        s = speak(voice, text, tmp)
        truth.append({"start": round(pos, 3), "end": round(pos + len(s) / SR, 3), "speaker": who})
        gap = rng.uniform(0.25, 0.9)
        audio += s + [0.0] * int(gap * SR)
        pos += len(s) / SR + gap
    audio = [x + rng.gauss(0, 0.002) for x in audio]
    write_wav(out / f"{name}.wav", audio)
    (out / f"{name}.json").write_text(json.dumps(truth, indent=1))


def main() -> None:
    out = Path(sys.argv[1]); out.mkdir(parents=True, exist_ok=True)
    rng = random.Random(11)
    with tempfile.TemporaryDirectory() as t:
        tmp = Path(t)
        for stem, va, vb, ta, tb in VOICES:
            for part, voice, text in (("a", va, ta), ("b", vb, tb)):
                s = speak(voice, text, tmp)
                write_wav(out / f"{stem}_{part}.wav", [x + rng.gauss(0, 0.002) for x in [0.0] * 8000 + s + [0.0] * 8000])
        meeting(out, "meet1", MEET1, tmp, rng)
        meeting(out, "meet2", MEET2, tmp, rng)
    (out / "voices.json").write_text(json.dumps([v[0] for v in VOICES]))
    print(f"wrote {len(VOICES)} voices and two meetings to {out}")


if __name__ == "__main__":
    main()

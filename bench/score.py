#!/usr/bin/env python3
"""Score transcripts against the ground truth from synth.py. Standard library only.

    python3 bench/score.py words TRUTH.json HYP.json   # word start/end error
    python3 bench/score.py der   TRUTH.json HYP.json   # diarization error rate
    python3 bench/score.py wer   TRUTH.txt  HYP.json   # word error rate + speaker attribution (with --turns TRUTH.json)

HYP.json is `lr transcribe --json` / `lr diarize --json` output, or a Scribely
beta project file (`*_transkribiert.json`).
"""
import argparse, collections, difflib, itertools, json, re, statistics


def norm_word(w):
    return re.sub(r"[^\wäöüß]", "", w.lower().replace("ß", "ss"))


def turns_of(doc):
    """[(start, end, speaker, text, words)] from either output format."""
    rows = doc["turns"] if isinstance(doc, dict) else doc
    out = []
    for t in rows:
        words = [(w.get("word") or w.get("text", ""), w["start"], w["end"]) for w in t.get("words", [])]
        out.append((t["start"], t["end"], t.get("speaker") or t.get("speaker_id"), t.get("text", ""), words))
    return out


def p90(v):
    v = sorted(v); return v[min(int(len(v) * 0.9), len(v) - 1)]


def cmd_words(truth, hyp):
    tw = [(norm_word(w["word"]), w["start"], w["end"]) for w in truth]
    hw = [(norm_word(w), s, e) for *_, words in turns_of(hyp) for w, s, e in words]
    sm = difflib.SequenceMatcher(a=[w for w, _, _ in tw], b=[w for w, _, _ in hw], autojunk=False)
    ds, de, bias = [], [], []
    for b in sm.get_matching_blocks():
        for k in range(b.size):
            t, h = tw[b.a + k], hw[b.b + k]
            ds.append(abs(t[1] - h[1])); de.append(abs(t[2] - h[2])); bias.append(h[1] - t[1])
    print(f"matched {len(ds)}/{len(tw)} words | start error median {statistics.median(ds)*1000:.0f} ms, "
          f"p90 {p90(ds)*1000:.0f} ms | end error median {statistics.median(de)*1000:.0f} ms | "
          f"start bias {statistics.mean(bias)*1000:+.0f} ms")


def der(ref, hyp, collar, step=0.01):
    n = int(max(t[1] for t in ref + hyp) / step) + 1
    def lab(turns):
        out = [None] * n
        for s, e, spk in turns:
            for i in range(int(s / step), min(int(e / step), n)): out[i] = spk
        return out
    r, h = lab(ref), lab(hyp)
    skip = set()
    for s, e, _ in ref:
        for edge in (s, e):
            skip.update(range(max(0, int((edge - collar) / step)), min(n, int((edge + collar) / step) + 1)))
    rs, hs = sorted({x for x in r if x}), sorted({x for x in h if x})
    best = None
    for perm in itertools.permutations(hs + [None] * max(0, len(rs) - len(hs)), len(rs)):
        m = dict(zip(perm, rs)); miss = fa = conf = total = 0
        for i in range(n):
            if collar and i in skip: continue
            if r[i]: total += 1
            if r[i] and not h[i]: miss += 1
            elif h[i] and not r[i]: fa += 1
            elif r[i] and h[i] and m.get(h[i]) != r[i]: conf += 1
        cand = ((miss + fa + conf) / total, miss / total, fa / total, conf / total)
        best = min(best, cand) if best else cand
    return best


def cmd_der(truth, hyp):
    ref = [(t["start"], t["end"], t["speaker"]) for t in truth]
    h = [(s, e, spk) for s, e, spk, *_ in turns_of(hyp)]
    for collar in (0.0, 0.25):
        d, mi, fa, co = der(ref, h, collar)
        print(f"collar {collar:.2f} s: DER {d*100:.1f}% (miss {mi*100:.1f}, false alarm {fa*100:.1f}, "
              f"confusion {co*100:.1f}), {len({x[2] for x in h})} speakers")


def wer(ref, hyp):
    d = list(range(len(hyp) + 1))
    for i, r in enumerate(ref, 1):
        prev, d[0] = d[0], i
        for j, hh in enumerate(hyp, 1):
            prev, d[j] = d[j], min(d[j] + 1, d[j - 1] + 1, prev + (r != hh))
    return d[len(hyp)] / len(ref)


def cmd_wer(text, hyp, truth_turns):
    split = lambda x: re.sub(r"[^\wäöüß ]", " ", x.lower().replace("ß", "ss")).split()
    turns = turns_of(hyp)
    line = f"WER {wer(split(text), [w for *_, txt, _ in turns for w in split(txt)])*100:.1f}%"
    if truth_turns:
        overlap = collections.defaultdict(collections.Counter)
        for t in truth_turns:
            for s, e, spk, *_ in turns:
                ov = min(e, t["end"]) - max(s, t["start"])
                if ov > 0: overlap[t["speaker"]][spk] += ov
        owner, used = {}, set()
        for ref_spk, c in sorted(overlap.items(), key=lambda kv: -sum(kv[1].values())):
            for spk, _ in c.most_common():
                if spk not in used: owner[ref_spk] = spk; used.add(spk); break
        right = sum(c[owner.get(r)] for r, c in overlap.items())
        line += f" | speaking time with the right speaker {right / sum(sum(c.values()) for c in overlap.values())*100:.1f}%"
    print(line + f" | {len(turns)} turns")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("kind", choices=["words", "der", "wer"])
    ap.add_argument("truth"); ap.add_argument("hyp")
    ap.add_argument("--turns", help="conv.json, for speaker attribution with `wer`")
    a = ap.parse_args()
    hyp = json.load(open(a.hyp))
    if a.kind == "wer":
        cmd_wer(open(a.truth).read(), hyp, json.load(open(a.turns)) if a.turns else None)
    else:
        {"words": cmd_words, "der": cmd_der}[a.kind](json.load(open(a.truth)), hyp)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Builds Resources/catalog.json (and Resources/icons/) from ollama.com/library.

Ollama has no public library API, so this reads the library and tags pages.
Run it now and then to refresh the models the app recommends:

    scripts/update-catalog.py
"""

import concurrent.futures
import datetime
import html
import json
import os
import re
import subprocess
import urllib.request

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "Resources")
BASE = "https://ollama.com"

# Library name prefix → Hugging Face organisation, for the maker's icon.
# Keep in sync with ModelIdentity.swift.
MAKERS = {
    "qwen": "Qwen", "qwq": "Qwen", "gemma": "google", "codegemma": "google", "embeddinggemma": "google",
    "llama": "meta-llama", "codellama": "meta-llama",
    "mistral": "mistralai", "mixtral": "mistralai", "devstral": "mistralai", "magistral": "mistralai",
    "ministral": "mistralai", "codestral": "mistralai",
    "gpt-oss": "openai", "deepseek": "deepseek-ai", "phi": "microsoft", "glm": "zai-org",
    "granite": "ibm-granite", "olmo": "allenai", "smollm": "HuggingFaceTB", "nemotron": "nvidia",
    "command": "CohereLabs", "aya": "CohereLabs", "falcon": "tiiuae", "kimi": "moonshotai",
    "minimax": "MiniMaxAI", "apertus": "swiss-ai", "hermes": "NousResearch", "lfm": "LiquidAI",
    "exaone": "LGAI-EXAONE", "starcoder": "bigcode", "minicpm": "openbmb", "internlm": "internlm",
    "yi": "01-ai", "seed": "ByteDance-Seed", "cogito": "deepcogito", "nomic": "nomic-ai",
}

CODING = re.compile(r"cod(e|er|ing)|devstral|codestral|starcoder|programm", re.I)
WRITING = re.compile(r"writ|creativ|story|roleplay|translat|multilingual|language", re.I)
RESEARCH = re.compile(r"reason|research|math|science|analy|think|r1\b", re.I)


def fetch(path):
    request = urllib.request.Request(BASE + path, headers={"User-Agent": "LocalRouter catalog builder"})
    with urllib.request.urlopen(request, timeout=30) as response:
        return response.read().decode("utf-8")


def parse_size(text):
    number, unit = re.match(r"([\d.]+)\s*([KMGT]?B)", text).groups()
    return int(float(number) * {"B": 1, "KB": 1e3, "MB": 1e6, "GB": 1e9, "TB": 1e12}[unit])


def parse_count(text):
    match = re.match(r"([\d.]+)([KMB]?)", text.strip())
    if not match:
        return 0
    number, unit = match.groups()
    return int(float(number) * {"": 1, "K": 1e3, "M": 1e6, "B": 1e9}[unit])


def library():
    page = fetch("/library?sort=popular")
    entries = []
    for block in re.findall(r'<li\s+class="flex items-baseline.*?</li>', page, re.S):
        name = re.search(r'href="/library/([^"]+)"', block).group(1)
        description = re.search(r'<p class="max-w-lg[^"]*">(.*?)</p>', block, re.S)
        chips = [c.strip() for c in re.findall(r'<span\s+class="inline-flex items-center rounded-md[^"]*">([^<]+)</span>', block)]
        numbers = re.findall(r'<span >([^<]+)</span>\s*<span class="hidden sm:flex">&nbsp;Pulls', block)
        updated = re.search(r'title="([A-Z][a-z]{2} \d+, \d{4})[^"]*"', block)
        entries.append({
            "name": name,
            "description": html.unescape(description.group(1).strip()) if description else "",
            "capabilities": [c for c in chips if c in ("tools", "thinking", "vision", "audio", "embedding")],
            "cloud": "cloud" in chips,
            "pulls": parse_count(numbers[0]) if numbers else 0,
            "updated": datetime.datetime.strptime(updated.group(1), "%b %d, %Y").date().isoformat() if updated else None,
        })
    return entries


# Variants the app offers: the default build per size (e.g. `9b`, `35b-a3b`)
# and its MLX build (e.g. `9b-mlx`). Quantisation variants are left out.
DEFAULT_TAG = re.compile(r"^(latest|e?\d+(\.\d+)?[bm](-a\d+(\.\d+)?b)?)$")
MLX_TAG = re.compile(r"^(e?\d+(\.\d+)?[bm](-a\d+(\.\d+)?b)?)-mlx$")


def variants(name):
    page = fetch(f"/library/{name}/tags")
    rows = re.findall(
        r'<a href="/library/([^"]+)" class="md:hidden.*?<span class="font-mono">\s*([0-9a-f]+)</span>\s*•\s*([\d.]+\s*[KMGT]?B)\s*•\s*([^•<]+?)\s*•\s*'
        r'<span class="hidden sm:inline">\s*([^•<]+?)\s*•', page, re.S)
    result, seen_sizes, seen_digests = [], {}, set()
    for full, digest, size, context, inputs in rows:
        tag = full.split(":", 1)[1]
        is_mlx = bool(MLX_TAG.match(tag))
        if not (DEFAULT_TAG.match(tag) or is_mlx):
            continue
        context_match = re.match(r"([\d.]+)([KM]?)", context)
        context_tokens = int(float(context_match.group(1)) * {"": 1, "K": 1024, "M": 1024 * 1024}[context_match.group(2)]) if context_match else None
        variant = {"tag": tag, "size": parse_size(size), "context": context_tokens, "mlx": is_mlx,
                   "inputs": [i.strip().lower() for i in inputs.replace(" input", "").split(",")]}
        if tag == "latest":
            seen_sizes["latest"] = variant["size"]
            continue
        # Several tags can point at the same build (e.g. `35b` and `35b-a3b`); keep the first.
        if digest in seen_digests:
            continue
        seen_digests.add(digest)
        result.append(variant)
    # Mark which size `latest` points at, then drop `latest` itself.
    for variant in result:
        variant["isDefault"] = not variant["mlx"] and variant["size"] == seen_sizes.get("latest")
    return result


def maker(name):
    for prefix in sorted(MAKERS, key=len, reverse=True):
        if name.startswith(prefix):
            return MAKERS[prefix]
    return None


def uses(entry):
    text = entry["name"] + " " + entry["description"]
    caps = entry["capabilities"]
    if "embedding" in caps:
        return []
    result = set()
    if CODING.search(text):
        result.add("coding")
    if "thinking" in caps or RESEARCH.search(text):
        result.add("researching")
    if "tools" in caps:
        result.add("working")
    if "coding" not in result or "tools" in caps:
        result.update(["talking", "writing"] if not re.search(r"ocr|guard|embed", text, re.I) else [])
    if WRITING.search(text):
        result.add("writing")
    return sorted(result)


def download_icon(org):
    path = os.path.join(ROOT, "icons", f"{org}.png")
    if os.path.exists(path):
        return True
    try:
        with urllib.request.urlopen(f"https://huggingface.co/api/organizations/{org}/avatar", timeout=20) as response:
            url = json.load(response)["avatarUrl"]
        raw = path + ".download"
        urllib.request.urlretrieve(url, raw)
        # 128 px PNG, so every icon has the same format and a small size.
        subprocess.run(["sips", "-s", "format", "png", "-Z", "128", raw, "--out", path], check=True, capture_output=True)
        os.remove(raw)
        return True
    except Exception as error:  # noqa: BLE001 — a missing icon falls back to a monogram
        print(f"  no icon for {org}: {error}")
        return False


def main():
    os.makedirs(os.path.join(ROOT, "icons"), exist_ok=True)
    entries = [e for e in library() if "embedding" not in e["capabilities"]]
    print(f"{len(entries)} library models")

    def build(entry):
        try:
            entry["variants"] = variants(entry["name"])
        except Exception as error:  # noqa: BLE001
            print(f"  skipped {entry['name']}: {error}")
            entry["variants"] = []
        return entry

    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        entries = list(pool.map(build, entries))

    models = []
    for entry in entries:
        if not entry["variants"]:
            continue  # cloud-only or unparsable
        entry["maker"] = maker(entry["name"])
        entry["uses"] = uses(entry)
        if any("image" in v["inputs"] for v in entry["variants"]) and "vision" not in entry["capabilities"]:
            entry["capabilities"].append("vision")
        del entry["cloud"]
        models.append(entry)

    for org in sorted({m["maker"] for m in models if m["maker"]}):
        download_icon(org)

    catalog = {"generatedAt": datetime.date.today().isoformat(), "models": models}
    with open(os.path.join(ROOT, "catalog.json"), "w") as file:
        json.dump(catalog, file, indent=1)
    print(f"Wrote {len(models)} models to Resources/catalog.json")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Self-correction eval: run selfcorr.jsonl through the offline rule and/or the
Apple Intelligence prompt, grade with keep/drop tokens.

  selfcorr.py run LABEL [--prompt FILE] [--offline-only] [--raw] [--runs N]
      offline-only: apply_scratch_that alone; --raw: LLM on the raw input
      (default: offline rule, then LLM, as the app does).
      Writes runs/selfcorr/LABEL.rN.jsonl.
  selfcorr.py grade LABEL...     per-kind score, per-case k/N, guard breaks

A case passes when every `keep` item (alternatives split by |) and no `drop`
item occurs as a whole word sequence in the lowercased output.
"""
import json
import os
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
CASES = HERE / "selfcorr.jsonl"
OUT = HERE / "runs" / "selfcorr"
SRC_TAURI = HERE.parent.parent / "src-tauri"
KINDS = ["cue", "scratch", "bare", "guard", "asr_garbled"]


def cases():
    return [json.loads(l) for l in CASES.read_text().splitlines() if l.strip()]


def norm(text):
    text = text.lower().replace("’", "'").replace("'", " ")
    # Keep the separators inside 10:30 / 3.000; drop other punctuation.
    text = re.sub(r"(?<!\d)[.:](?!\d)|[,;!?\"()\-–—*$€£]", " ", text)
    return " " + " ".join(text.split()) + " "


def has(out, item):
    return any(
        (alt in out) if alt == "?" else (" " + " ".join(norm(alt).split()) + " ") in out
        for alt in item.split("|")
    )


def passes(case, output):
    n = norm(output) + (" ? " if "?" in output else "")
    return all(has(n, k) for k in case["keep"]) and not any(has(n, d) for d in case["drop"])


def run(label, prompt=None, offline_only=False, raw=False, runs=1):
    OUT.mkdir(parents=True, exist_ok=True)
    inputs = OUT / "inputs.txt"
    inputs.write_text("\n".join(c["input"] for c in cases()) + "\n")
    env = dict(os.environ, BACKTRACK_INPUTS_FILE=str(inputs), CMAKE_POLICY_VERSION_MINIMUM="3.5")
    env["BACKTRACK_SCRATCH"] = "0" if raw else "1"
    env["BACKTRACK_NO_LLM"] = "1" if offline_only else "0"
    if prompt:
        env["BACKTRACK_PROMPT_FILE"] = str(Path(prompt).resolve())
    for r in range(1, runs + 1):
        res = subprocess.run(
            ["cargo", "test", "--lib", "backtrack_prompt_with_apple", "--", "--ignored", "--nocapture"],
            cwd=SRC_TAURI, env=env, capture_output=True, text=True,
        )
        rows = [json.loads(l[len("SELFCORR "):]) for l in res.stdout.splitlines() if l.startswith("SELFCORR ")]
        if len(rows) != len(cases()):
            sys.exit(f"run {r}: got {len(rows)} rows\n{res.stdout[-2000:]}\n{res.stderr[-2000:]}")
        path = OUT / f"{label}.r{r}.jsonl"
        path.write_text("".join(json.dumps(x, ensure_ascii=False) + "\n" for x in rows))
        print(f"wrote {path}")


def grade(labels):
    cs = cases()
    for label in labels:
        files = sorted(OUT.glob(f"{label}.r*.jsonl"))
        runs = [[json.loads(l) for l in f.read_text().splitlines()] for f in files]
        n = len(runs)
        print(f"\n## {label} ({n} run{'s' * (n > 1)})\n")
        per_kind = {}
        for i, c in enumerate(cs):
            k = sum(passes(c, r[i]["out"]) for r in runs)
            per_kind.setdefault(c["kind"], []).append(k)
        print("| kind | pass (case-runs) |\n|---|---|")
        for kind in KINDS:
            ks = per_kind.get(kind, [])
            print(f"| {kind} | {sum(ks)}/{len(ks) * n} |")
        scored = [k for kind in ("cue", "scratch", "bare", "guard") for k in per_kind.get(kind, [])]
        print(f"| **scored (no asr_garbled)** | **{sum(scored)}/{len(scored) * n}** |")
        print("\nFailing cases (k/N, last output):\n")
        for i, c in enumerate(cs):
            k = sum(passes(c, r[i]["out"]) for r in runs)
            if k < n:
                out = runs[-1][i]["out"].replace("\n", " ⏎ ")
                print(f"- {c['id']} [{c['kind']}] {k}/{n}: {out}")


if __name__ == "__main__":
    a = sys.argv[1:]
    if a and a[0] == "run":
        prompt = a[a.index("--prompt") + 1] if "--prompt" in a else None
        runs = int(a[a.index("--runs") + 1]) if "--runs" in a else 1
        run(a[1], prompt, "--offline-only" in a, "--raw" in a, runs)
    elif a and a[0] == "grade":
        grade(a[1:])
    else:
        print(__doc__)

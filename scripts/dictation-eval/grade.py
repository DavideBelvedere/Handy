"""Grade a run (exact / loose / fail on the text Handy typed), measure auto-learn, or show the trend across runs."""
import difflib, json, pathlib, re, sys
from collections import defaultdict

D = pathlib.Path(__file__).parent
STORE = pathlib.Path.home() / "Library/Application Support/com.pais.handy/settings_store.json"

def loose(t): return re.findall(r"[\w%$:]+", t.lower())
def norm(t): return " ".join(loose(t))

def end_pairs(path):
    """Learned pairs at the end of the run (runs/<run>.end.json, written by the server), else the current store."""
    end = path.with_suffix(".end.json")
    if end.exists():
        return [(norm(f), norm(t)) for f, t, l in json.loads(end.read_text())["pairs"] if l]
    s = json.loads(STORE.read_text()); s = s.get("settings", s)
    return [(norm(r["from"]), norm(r["to"])) for r in s.get("word_replacements", []) if r.get("learned")]

def subs(a, b):
    """Word substitutions (<=3 words each side) between two texts: what a user fix looks like to auto-learn."""
    A, B = loose(a), loose(b)
    return [(" ".join(A[i1:i2]), " ".join(B[j1:j2])) for tag, i1, i2, j1, j2 in difflib.SequenceMatcher(None, A, B).get_opcodes()
            if tag == "replace" and i2 - i1 <= 3 and j2 - j1 <= 3]

def has_words(text, words):
    """`words` occurs in `text` as a word sequence, ignoring case and punctuation ("We expect" holds "we expect")."""
    return f" {norm(words)} " in f" {norm(text)} "

def verdict(r):
    typed, exp = r["as_inserted"].strip(), r["expected"].strip()
    v = "PASS" if typed == exp else "LOOSE" if loose(typed) == loose(exp) else "FAIL"
    return "FAIL" if r.get("term") and not has_words(typed, r["term"]) else v

def resembles(heard, phrase, floor=0.4):
    """Whether a captured transcription belongs to this phrase (word-level similarity), to reject a history row of another dictation."""
    return difflib.SequenceMatcher(None, loose(heard or ""), loose(phrase)).ratio() >= floor

def auto_fix(inserted, expected, term):
    """What a careful user would fix: only the words of the term (<=3-word substitution holding it); neighbouring words are left alone."""
    if not term: return inserted
    A, B, T = inserted.split(), expected.split(), set(norm(term).split())
    out = []
    for tag, i1, i2, j1, j2 in difflib.SequenceMatcher(None, [norm(w) for w in A], [norm(w) for w in B]).get_opcodes():
        if not (tag == "replace" and i2 - i1 <= 3 and j2 - j1 <= 3 and T <= set(norm(" ".join(B[j1:j2])).split())):
            out += A[i1:i2]
        elif i2 - i1 == j2 - j1:  # same length: swap only the term's words
            out += [b if set(norm(b).split()) & T else a for a, b in zip(A[i1:i2], B[j1:j2])]
        else:
            out += B[j1:j2]
    return " ".join(out)

def live_stats(r, path):
    ev = [e for e in r["events"] if "v" in e and e["t"] <= r["ins_t"]]  # only what arrived before the user froze the text
    log = path.with_suffix("") / "logs" / f"{r['id']}.log"
    recs = log.read_text().count("Recording stopped and samples") if log.exists() else "-"
    return len(ev), (ev[-1]["t"] if ev else 0), sum(1 for a, b in zip(ev, ev[1:]) if len(b["v"]) < len(a["v"])), recs

def analyze(path):
    lines = [json.loads(l) for l in path.read_text().splitlines() if l.strip()]
    meta, rows = lines[0]["meta"], [r for r in lines[1:] if not r["skipped"]]
    start = {(norm(f), norm(t)) for f, t, l in meta["start"]["pairs"] if l}
    end = set(end_pairs(path))
    new = end - start
    fixes = {s for r in rows for s in subs(r["as_inserted"], r["typed"]) if s[0] and s[1]}
    # A fix counts as learned when a pair covers it: "Okonkwo-Brandt" splits into two words, so the pair can be longer than the fix.
    covered = lambda fx, pairs: any(f" {fx[0]} " in f" {f} " and f" {fx[1]} " in f" {t} " for f, t in pairs)
    learned = {fx for fx in fixes if covered(fx, end)}
    by_cat = defaultdict(lambda: defaultdict(int))
    for r in rows: by_cat[r["category"]][verdict(r)] += 1
    return dict(path=path, meta=meta, rows=rows, by_cat=by_cat, start=start, end=end, new=new, fixes=fixes,
                learned=learned, misses=fixes - learned, good_new={p for p in new if covered(p, fixes) or any(covered(fx, [p]) for fx in fixes)})

def acc(by_cat, *cats):
    n = sum(sum(by_cat[c].values()) for c in cats)
    ok = sum(by_cat[c]["PASS"] + by_cat[c]["LOOSE"] for c in cats)
    return f"{100 * ok // n}% ({ok}/{n})" if n else "-"

def report(a):
    out = [f"# {a['path'].name}", f"flags: {a['meta']['start']['flags']}", "",
           "| category | PASS | LOOSE | FAIL |", "|---|---|---|---|"]
    out += [f"| {c} | {d['PASS']} | {d['LOOSE']} | {d['FAIL']} |" for c, d in a["by_cat"].items()]
    known = [r for r in a["rows"] if "raw_asr" in r and not r.get("capture_error") and (not r["raw_asr"] or resembles(r["raw_asr"], r["phrase"]))]
    pp, suite = sum(1 for r in known if r.get("handy_pp")), a["meta"]["label"]
    warn = {"pp": pp < len(known), "plain": pp > 0}.get(suite, False)
    out += ["", f"**LLM post-processing ran on {pp}/{len(known)} phrases with a captured history entry** ({suite} suite" + (": WRONG shortcut, plain = option+space, pp = option+shift+space)" if warn else ")")]
    prompt = a["meta"]["start"]["flags"].get("post_process_selected_prompt_id")
    if suite == "pp" and prompt != "default_backtrack":
        out += [f"**Prompt under test is {f'`{prompt}`' if prompt else 'not recorded (run predates the snapshot field)'}, not `default_backtrack`**: "
                "select \"Clean + resolve self-corrections\" in Settings > Post-processing for a pp run."]
    bad = [r["id"] for r in a["rows"] if r.get("capture_error")]
    if bad: out += [f"Capture rejected for {bad} (history entry missing or from another dictation)."]
    out += [
            "", "## Auto-learn",
            f"- user fixes (word substitutions): {len(a['fixes'])}; learned: {len(a['learned'])}; missed: {sorted(a['misses'])}",
            f"- new learned pairs this run: {sorted(a['new'])} ({len(a['good_new'])} match a user fix)",
            "(the last phrase's fix is learned when the field loses focus; grade after clicking away)", "",
            "| id | category | verdict | live: events / span ms / shrinks | recordings |", "|---|---|---|---|---|"]
    out += [f"| {r['id']} | {r['category']} | {verdict(r)} | {'{} / {} / {} | {}'.format(*live_stats(r, a['path']))} |" for r in a["rows"]]
    out += ["(recordings = dictations Handy logged while the phrase was shown; more than 1 = restarted, not a clean sample. "
            "Logs of runs before 2026-10-11 14:00 start 2 s early and may count the previous phrase.)"]
    out += ["", "## Non-pass"]
    out += [f"- **{r['id']}** [{verdict(r)}] said: {r['phrase']}\n  - expected: `{r['expected']}`\n  - inserted: `{r['as_inserted']}`" + (f"\n  - raw ASR:  `{r['raw_asr']}`" if r.get('raw_asr') else "")
            for r in a["rows"] if verdict(r) != "PASS"]
    return "\n".join(out)

def trend():
    out = ["| run | flags (live/pp/learn) | n | overall | newterm (control) | recurring (probe) | custom+mapped | learned pairs at start | new | recall |", "|---|---|---|---|---|---|---|---|---|---|"]
    for p in sorted((D / "runs").glob("*.jsonl")):
        a = analyze(p); f, c = a["meta"]["start"]["flags"], a["by_cat"]
        flags = "/".join("y" if f.get(k) else "n" for k in ("live_typing_enabled", "post_process_enabled", "auto_learn_enabled"))
        out.append(f"| {p.stem} | {flags} | {len(a['rows'])} | {acc(c, *c)} | {acc(c, 'newterm')} | {acc(c, 'recurring')} | {acc(c, 'custom', 'mapped')} "
                   f"| {len(a['start'])} | {len(a['new'])} | {len(a['learned'])}/{len(a['fixes'])} |")
    return "\n".join(out)

def cases():
    """Every captured dictation as a self-contained case: audio, raw ASR, what Handy typed, expected. Handoff fixture for whoever improves the code."""
    out, claims, dropped = [], {}, []
    for p in sorted((D / "runs").glob("*.jsonl")):
        a = analyze(p)
        for r in a["rows"]:
            if r.get("capture_error") or (not r.get("raw_asr") and not r.get("audio")): continue
            if r.get("raw_asr") and not resembles(r["raw_asr"], r["phrase"]):
                dropped.append(f"{p.stem}:{r['id']}"); continue  # history row of another dictation
            c = {"run": p.name, "id": r["id"], "suite": a["meta"]["label"], "category": r["category"],
                 "phrase": r["phrase"], "expected": r["expected"], "term": r.get("term"), "raw_asr": r.get("raw_asr"),
                 "pp_ran": bool(r.get("handy_pp")), "handy_pp": r.get("handy_pp"), "as_inserted": r["as_inserted"], "verdict": verdict(r),
                 "audio": str(D / r["audio"]) if r.get("audio") else None}
            # Same recording claimed by two phrases: keep the phrase it resembles most.
            key = r.get("history_id") or id(c)
            score = difflib.SequenceMatcher(None, loose(c["raw_asr"] or ""), loose(c["phrase"])).ratio()
            if key not in claims or score > claims[key][0]: claims[key] = (score, c)
    out = [c for _, c in claims.values()]
    (D / "cases.jsonl").write_text("\n".join(json.dumps(c, ensure_ascii=False) for c in out) + "\n")
    return f"{len(out)} cases -> cases.jsonl" + (f" (dropped, transcript of another dictation: {dropped})" if dropped else "")

def asr():
    """Headless regression check, no human, no Chrome: re-run every saved WAV in cases.jsonl through the current
    build (`handy --transcribe-file --stream`: streaming ASR + scratch-that + custom words + replacements + fillers, no LLM, no live
    typing) and compare with the raw_asr captured at dictation time. Uses the live settings (learned pairs included)."""
    import os, subprocess, time
    from collections import Counter
    binary = os.environ.get("HANDY_BIN", str(D.parents[1] / "src-tauri/target/debug/handy"))
    rows, out = [json.loads(l) for l in (D / "cases.jsonl").read_text().splitlines() if l.strip()], []
    tally = defaultdict(Counter)
    for c in rows:
        if not c.get("audio") or not pathlib.Path(c["audio"]).exists(): continue
        # --stream = the app's streaming decode (no live typing without --live-type); batch decode punctuates differently
        # cwd: a debug build writes ../src/bindings.ts relative to it, which is only right from src-tauri/
        # ASR_REALTIME=1 feeds the WAV at mic speed (slow: ~1x audio length), the app's timing
        flags = ["--stream", "--json"] + (["--realtime"] if os.environ.get("ASR_REALTIME") else [])
        p = subprocess.run([binary, "--transcribe-file", c["audio"], *flags], capture_output=True, text=True, timeout=300,
                           cwd=D.parents[1] / "src-tauri")
        if p.returncode: out.append(f"- **{c['id']}** handy exited {p.returncode}: {p.stderr[-200:]}"); continue
        new = json.loads(p.stdout.strip().splitlines()[-1])["text"]
        before = verdict({**c, "as_inserted": c["raw_asr"] or ""})
        after = verdict({**c, "as_inserted": new})
        # pp expected assumes the LLM ran: only the text change is meaningful there
        key = "pp (pre-LLM)" if c["suite"] == "pp" else c["category"]
        tally[key][f"{before}->{after}"] += 1
        if new != (c["raw_asr"] or ""):
            out.append(f"- **{c['run'][:15]} {c['id']}** [{c['category']}] {before} -> {after}\n  - was: `{c['raw_asr']}`\n  - now: `{new}`\n  - expected: `{c['expected']}`")
    rank = {"PASS": 2, "LOOSE": 1, "FAIL": 0}
    better = sum(n for t in tally.values() for k, n in t.items() if rank[k.split("->")[1]] > rank[k.split("->")[0]])
    worse = sum(n for t in tally.values() for k, n in t.items() if rank[k.split("->")[1]] < rank[k.split("->")[0]])
    text = "\n".join([f"# ASR replay {time.strftime('%Y%m%d-%H%M%S')} ({binary})", "",
                      f"**{better} better, {worse} worse** (verdict of the captured raw_asr -> verdict of the current build)", "",
                      "| category | verdict before -> after |", "|---|---|"]
                     + [f"| {k} | {', '.join(f'{t}: {n}' for t, n in sorted(v.items()))} |" for k, v in sorted(tally.items())]
                     + ["", "## Text that changed"] + (out or ["(none)"]))
    path = D / "runs" / f"asr-{time.strftime('%Y%m%d-%H%M%S')}.md"
    path.write_text(text)
    return text + f"\n\n-> {path.relative_to(D)}"

def selftest():
    v = lambda ins, exp, term=None: verdict({"as_inserted": ins, "expected": exp, "term": term})
    assert v("We expect rain.", "We expect rain.", "we expect") == "PASS"  # term match ignores case
    assert v("TrellisDB stores it ", "Trellisdb stores it.", "Trellisdb") == "LOOSE"
    assert v("Trellis stores it.", "Trellisdb stores it.", "Trellisdb") == "FAIL"
    assert v("send it to Bobby", "send it to Bob", "Bob") == "FAIL"  # whole words only
    assert resembles("the garage door needs a new spring", "The garage door needs a new spring. Scratch that.")
    assert not resembles("The Rental Vampicup is at seven on Saturday", "The garage door needs a new spring. Scratch that.")
    return "ok"

if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--selftest":
        print(selftest())
    elif len(sys.argv) > 1 and sys.argv[1] == "--asr":
        print(asr())
    elif len(sys.argv) > 1 and sys.argv[1] == "--cases":
        print(cases())
    elif len(sys.argv) > 1 and sys.argv[1] == "--trend":
        print(trend())
    else:
        path = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 and sys.argv[1] else sorted((D / "runs").glob("*.jsonl"))[-1]
        text = report(analyze(path)); path.with_suffix(".report.md").write_text(text); print(text)

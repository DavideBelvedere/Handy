"""Ask Haiku for fresh test phrases + expected text, seeded with the user's custom words and learned mappings."""
import json, pathlib, random, re, subprocess, sys
import grade

D = pathlib.Path(__file__).parent
STORE = pathlib.Path.home() / "Library/Application Support/com.pais.handy/settings_store.json"
s = json.loads(STORE.read_text())
s = s.get("settings", s)
words = s.get("custom_words", [])
# only these keys are read: the store also holds API keys
maps = [(r["from"], r["to"]) for r in s.get("word_replacements", []) if not r.get("learned")]
learned = [(r["from"], r["to"]) for r in s.get("word_replacements", []) if r.get("learned")]
# Learned pairs whose `from` is an ordinary word rewrite normal speech: guard phrases use that word as meant.
guards = [f for f, t in learned if f.islower()][:6]
# Terms that failed first-try in earlier runs come back in fresh sentences: that is the auto-learn probe.
recurring = set()
for f in (D / "runs").glob("*.jsonl"):
    for line in f.read_text().splitlines():
        r = json.loads(line)
        if r.get("term") and r["category"] in ("newterm", "recurring") and not r.get("skipped") and not grade.has_words(r.get("as_inserted", r.get("typed", "")), r["term"]):
            recurring.add(r["term"])
recurring = sorted(recurring)[:10]
words = random.sample(words, min(6, len(words)))
topics = random.sample(
    ["insurance claims", "travel plans", "cooking", "software bugs", "football", "meetings", "shopping", "health",
     "weather", "finance", "music", "home repairs", "school", "customer support", "data pipelines", "holidays"], 6)

PROMPT = f"""You generate test phrases for a speech-to-text dictation app. A human READS each phrase aloud, the app transcribes it (Parakeet ASR) and types it into a text box, and a grader compares the box with `expected`.
Return ONLY a JSON array of {{"id": string, "category": string, "suite": "plain"|"pp", "phrase": string, "expected": string, "term": string|null}}.
`expected` = ideal final text: punctuation/capitalization fixed, numbers as digits, fillers removed, self-corrections resolved. Topics for variety: {", ".join(topics)}. Ids unique (b1, s1, ...).

SUITE "plain" (read with plain dictation: ASR + custom words/replacements + built-in filler removal + the "scratch that" command, no LLM). SHORT phrases, 6-20 words:
- newterm (6): each sentence has ONE rare name an ASR would likely misspell (invented-but-plausible surnames, startups, products, places; not common words). term = that name, exact spelling.
- recurring (2 per term): FRESH sentences (never the same wording) each containing one of: {json.dumps(recurring)}. term = the term. Skip if the list is empty.
- custom (one per word, term = the word, exact spelling/case): each sentence uses one of: {json.dumps(words)}.
- mapped (1 per pair, term = the `to` side): sentences using the `to` word naturally, so the ASR is tempted to output the `from` side. Pairs (from -> to): {json.dumps(maps)}. Skip if empty.
- guard (1 per word, term = the word): an ordinary sentence using the word with its normal meaning; expected keeps it unchanged (auto-learn must not rewrite it). Words: {json.dumps(guards)}. Skip if empty.
- filler (2): "um" / "uh" only (plain dictation keeps "like", which can be a real word) -> removed.
- scratch (4): a full sentence, then "scratch that" (1) or "delete that" (1) as its own clause, then a replacement sentence; one more where the replacement repeats the start of the dropped sentence ("The bus leaves at seven scratch that the bus leaves at eight", expected "The bus leaves at eight."); last: a sentence then "scratch that" and nothing follows (expected empty string).

SUITE "pp" (read with dictation + LLM cleanup that resolves self-corrections). LONGER phrases, 2-3 sentences, 25-45 words, term null:
- backtrack (6): corrections with "no", "no wait", "I mean", "sorry", "actually" (restated version kept, abandoned one dropped with the cue); mix days, times, names, quantities; one phrase may hold two corrections.
- scratch (3): "scratch that" / "delete that" after a sentence, inside a longer passage; expected = passage without the abandoned sentence.
- emphasis (2): "actually" as emphasis, NOT a correction -> kept; inside a longer passage.
- numbers (2): number words -> digits, percent, dollars, times.
- punct (2): spoken "comma", "period", "question mark" -> symbols.
- question (2): a question must be cleaned, never answered.
- mixed (3): several of the above in one passage (a correction + a number + a filler such as "like").
Every `expected` must spell each term exactly as given (case included).
Set "suite" per phrase accordingly."""

out = subprocess.run(["claude", "-p", "--model", "haiku"], input=PROMPT, capture_output=True, text=True, check=True).stdout
phrases = json.loads(out[out.index("["): out.rindex("]") + 1])
(D / "phrases.json").write_text(json.dumps(phrases, indent=1, ensure_ascii=False))
print(f"{len(phrases)} phrases -> phrases.json")

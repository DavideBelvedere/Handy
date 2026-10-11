# Dictation eval: handoff

Tooling to measure how well Handy turns speech into the right text, and to track whether changes (or auto-learn) improve it. Written for the agent whose job is to improve the code so these cases pass more often. A human must read phrases aloud for a full run; the rest can be done offline (see "Measuring without a human").

## Status (2026-10-11): retest pending

**The next step is a human retest of both suites; nothing after `408085c` has been verified by one.** Iteration 2 did not measure the intended setup (custom prompt selected, risky learned pairs still active; see "Iteration 2 results"), and the code fixes since then (`408085c` fuzzy matcher, the eval's freeze-on-history page) are checked only by unit tests and `eval.sh asr`.

Before the retest the user must, in the installed Handy (`scripts/install-macos.sh` builds and installs this checkout; grant Accessibility again after each install):
1. Settings > Post-processing: select "Clean + resolve self-corrections" (`default_backtrack`).
2. Settings > Word Replacements: delete `test -> text`, `with pet -> we expect`, `forecasted -> forecast` (in the UI: editing the settings file while Handy runs gets overwritten).

Then follow "Next iteration (checklist)", re-using the current `phrases.json` (skip `gen`) so the result compares directly with iteration 2. Check in the reports: the pp run says it used `default_backtrack`, `guard` passes, `recordings` is 1 per phrase, and q7/q8/q9 are no longer frozen before the final text. Update this section with the outcome.

## The two suites

A human dictates the same kind of phrases under two Handy shortcuts. Never mix them: the grader flags a run read with the wrong shortcut.

| Suite | Shortcut | Pipeline under test | Phrases |
|---|---|---|---|
| `plain` | `option+space` | ASR (Parakeet) + deterministic "scratch that" (`apply_scratch_that`, `audio_toolkit/text.rs`) + custom words (fuzzy, plus learned names) + learned replacements + built-in filler removal + live typing + auto-learn. **No LLM.** | short (6-20 words): `newterm`, `recurring`, `custom`, `mapped`, `guard`, `filler`, `scratch` |
| `pp` | `option+shift+space` | everything above + the LLM post-process prompt **selected in Settings** (must be "Clean + resolve self-corrections" = `default_backtrack`, `settings.rs`) | long (25-45 words): `backtrack`, `scratch`, `emphasis`, `numbers`, `punct`, `question`, `mixed` |

- `custom` and `mapped` stress words already in the user's settings (`custom_words`, manual `word_replacements`).
- `recurring` re-uses terms that failed first try in an earlier run, in fresh sentences: the auto-learn probe (`newterm` is the control).
- `guard` uses the `from` word of a learned pair with its normal meaning (e.g. "test" after `test -> text` was learned): it must come out unchanged, so it measures auto-learn misfires. The page never auto-fixes a guard phrase (that would teach the reverse pair).

## Commands (from the repo root)

```bash
./scripts/dictation-eval/eval.sh gen            # Haiku makes phrases + expected text for both suites
./scripts/dictation-eval/eval.sh serve plain    # opens Chrome; one suite per server run
./scripts/dictation-eval/eval.sh grade          # grade the latest run -> runs/<run>.report.md
./scripts/dictation-eval/eval.sh serve pp
./scripts/dictation-eval/eval.sh trend          # one row per run: accuracy per category, learned pairs, recall
./scripts/dictation-eval/eval.sh cases          # export every captured dictation -> cases.jsonl
./scripts/dictation-eval/eval.sh asr            # headless: re-run every saved WAV through the current build, before/after
./scripts/dictation-eval/eval.sh replay runs/<run>.jsonl   # re-feed the saved audio with live typing in Chrome
python3 scripts/dictation-eval/grade.py --selftest          # grader sanity checks
```

Reading a phrase: click the box, `option+space` (or `+shift`) to start, again to stop. Once Handy has saved the dictation to its history and 1 s passed without new text, the page compares with the expected text and shows PASS / LOOSE / FAIL (the page cannot see option+space: Handy grabs it). LOOSE = same words, different case/punctuation/spacing. Enter = next phrase. If you misread, press **Skip** rather than restarting the dictation: a restart leaves two recordings and an unclean sample (the report's `recordings` column counts the dictations Handy logged per phrase).

## What a run records

Per phrase: the text as inserted, the fixed text, timed input events, and from Handy's history the WAV, the pipeline text before the LLM (`raw_asr`) and the LLM output (`handy_pp`). A history entry that does not resemble the phrase (another dictation) is rejected with `capture_error` instead of copied. The Handy log lines of each phrase go to `runs/<run>/logs/<id>.log` (enigo's per-keystroke lines dropped): the log rotates after a few dictations. The settings snapshot holds the selected prompt id, threshold, trailing-space and history-limit settings.

## How auto-learn is exercised

Auto-learn (`correction_learner.rs`) learns from edits in the focused field. In a normal run the audio is **not** re-fed: for phrases with a `term`, the page itself rewrites the wrong word in the field (only the term's words, like a careful user would), waits for the watcher's 500 ms poll, then clears the box. `grade` then compares the user fixes with the learned pairs in the settings store (recall; a pair longer than the fix, e.g. "Okonkwo-Brandt", still covers it). Learning is *measured* three ways:

1. `recurring` vs `newterm` accuracy across runs (fresh audio, same terms).
2. `asr` / `replay` of the **same audio** after learning: the words that were fixed should now come out right.
3. `guard`: learned pairs must not rewrite ordinary words.

Since `1d29616`, a name-like target learned from 2+ different mishearings (Vandermeulen, Okonkwo-Brandt) joins the fuzzy custom-word matcher at runtime (`AppSettings::learned_names`), so a new spelling variant is caught without a new pair.

## Measuring without a human

- **`eval.sh asr` (preferred, ~1 s per clip, Handy can stay open):** runs `target/debug/handy --transcribe-file X.wav --stream --json` on every WAV in `cases.jsonl` and compares with the `raw_asr` captured at dictation time: verdict before -> after per category, plus every text that changed -> `runs/asr-<time>.md`. It covers streaming ASR, scratch-that, custom words, learned names/pairs and fillers with the **current** settings, not live typing and not the LLM. `--stream` matters: the batch decode punctuates differently and looks like a regression. Rebuild first (`cd src-tauri && cargo build`; a running `bun run tauri dev` rebuilds on its own). For `pp` cases only the pre-LLM text change is meaningful.
- **Deterministic stages:** `cases.jsonl` has one case per captured dictation with `raw_asr`, `expected`, `as_inserted`, `verdict`, `suite` (from the run), `pp_ran`, `audio`. Turn failing ones into Rust unit tests (`apply_scratch_that`, `apply_custom_words`, ...). Never change `expected`; if one looks wrong, tell the user.
- **LLM prompt:** the ignored test `backtrack_prompt_with_apple_intelligence` in `settings.rs`; `BACKTRACK_INPUTS_FILE` = one transcript per line (e.g. the `pp` cases' `raw_asr`), `BACKTRACK_PROMPT_FILE` = another prompt. Run it 3 times: the on-device model is not deterministic across prompts.
  ```bash
  cd src-tauri && BACKTRACK_INPUTS_FILE=/path/inputs.txt cargo test --lib backtrack_prompt_with_apple -- --ignored --nocapture
  ```
- **Live typing replay:** `eval.sh replay` (`--stream --live-type 6` into the Chrome page). Needs the Handy app quit (shared settings file) and Accessibility permission for the terminal. Never run end to end yet; `asr` covers everything but live typing.
- **Final check** is always a human run of both suites; compare with `eval.sh trend`. Trend/recall use `runs/<run>.end.json` (learned pairs at the end of the run, written by the server); for iteration 1 these were frozen from the store on 2026-10-10 22:00, so older runs' "new"/"recall" are approximate but no longer drift.

## Iteration 1 results (2026-10-10)

### Data corrections (read first)

- **Grader bug, fixed:** the term check was case-sensitive, so mp2 ("We expect" vs term "we expect"), rc5 and rc6 ("TrellisDB" vs "Trellisdb") were false FAILs. Re-graded: plain run `20261010-211350_plain` **10/31 -> 13/31**, `recurring` 0/8 -> 2/8, recall 9/11 -> 10/11. This is a grader change, not a product gain.
- **Mis-captured audio:** the history recovery gave sc3 (plain) the recording of x3 (pp), and s4 (baseline) the recording of s3. That is why the plain report said "LLM ran on 1/31: wrong shortcut": it was x3's entry. `cases` now drops a capture that does not resemble its phrase, and keeps a shared recording only for the phrase it resembles most.
- **The pp suite never tested the backtrack prompt.** The selected prompt was the user's custom `prompt_1791655929868` ("clean it."), whose own text even contains a dictated "scratch that". The x1/x2/x3 findings of the first pass are about that prompt. The report now warns when a pp run is not on `default_backtrack`.
- The expected spelling `Trellisdb` (Haiku's) is probably wrong: the product is spelled TrellisDB elsewhere. Not changed; the term check now ignores case.

### Fixed in code (commits on `feat/auto-learn-live-typing-backtrack`)

| Commit | Problem seen in the eval | Fix | Cases |
|---|---|---|---|
| `cc33416`, `16d26fa` | "scratch that"/"delete that" ignored when Parakeet punctuates neither side ("…on staging scratch that the pipeline runs…") | also a command when the words after the cue restate the dropped sentence (same first word + 1 more shared word in the next six for "scratch that", 2 for "delete that", so "I can delete that I can do it later" stays speech); the rest inherits the capital | s1, s2, s3, sc2, x3 (pre-LLM: "The pickup is at 80:30…") |
| `093728e` | custom word misfire "clause" -> "Claude" (1 edit in 6 letters < 0.18) | words up to 6 letters need matching Soundex consonants (first letter aside) | m1 |
| `1d29616` | `recurring` 0/8: each mishearing became its own exact pair | names learned from 2+ mishearings are fuzzy-matched | new variants of Vandermeulen / Okonkwo-Brandt (unit test); rc3 "Auconqu" is still too far |
| `166b854` | live typing erased the whole text and retyped it whenever the final differed ("193 typed, 193 backspaces" on every pp dictation) | delete and retype only the tail after the shared prefix, still after a full verify | sc1, all pp |
| `5d7888d` | backtrack prompt kept "scratch that" inside a passage, numbers in words | two examples; stored unedited default prompts get them on upgrade | x3 cue removed 3/3 but the wrong subject kept ("The Rental Vampicup is at 8:30": still FAIL); x1 numbers 3/3 (manual test). On real input x3 is now resolved before the LLM, so the example is a fallback |

`eval.sh asr` on the 36 captured clips after these commits: **5 better, 0 worse** (s1, s2, s3, m1 FAIL -> LOOSE by code; w1 by a pair learned during the baseline, not by code), x3's scratch resolved before the LLM.

### Still open

- **"sorry" corrections** (x2 "eighteen thousand dollars sorry twenty thousand") are not resolved by the on-device model with any prompt tried (0/9). A deterministic rule would be risky ("sorry" is common speech); a bigger model or a cloud provider is the real fix.
- **"no wait" heard as "Now wait"** (b2) and "Monday not Tuesday" (b1) defeat cue matching; the LLM sometimes keeps the first option ("Thursday, no Friday" -> Thursday).
- **Numbers / spoken punctuation in plain:** by design only the LLM converts them. Do not add them to the plain pipeline without asking.
- **Missing final period + trailing space in plain:** ASR output plus the user's `append_trailing_space`; LOOSE by design.
- **"like" as filler** is kept by plain dictation (it is a real word); `gen` no longer asks plain to remove it.
- **nt4 lost its first two words** ("Vellacourt Analytics sent…" -> "sent…"): the first live event already lacked them; no audio/log survived. Watch for it in the next run's logs.
- **Dictations restarted mid-phrase** (b5, mp5: text typed, erased, retyped word by word seconds later = a second streaming session): probably a human restart; the `recordings` column now shows it.
- **Risky learned pairs in the user's settings:** `test -> text`, `forecasted -> forecast`, `with pet -> we expect` rewrite ordinary speech everywhere. The new `guard` category will measure it; deleting them or guarding the learner against lowercase -> lowercase pairs is the user's call.
- **Learned-name fuzzy matching can misfire** on look-alike names (e.g. "Vanderbilt" -> "Vandermeulen"): same risk as any custom word; the misfire toast fires and the user can remove the learned pair.
- Handy's `history_limit` is 5: capture copies at save time, so it is enough during a run, but nothing older survives.

## Iteration 2 results (2026-10-11)

Runs `20261011-133909_plain` (34) and `20261011-134710_pp` (20). **Neither measured what it was meant to**, for two reasons:

- **The pp run used the custom prompt again** (`prompt_1791655929868`, the report says so): the backtrack prompt is still untested by a human run.
- **The 3 risky learned pairs were still active** (guard 3/3 FAIL: "test" -> "text", "with pet" -> "we expect", "forecasted" -> "forecast"): Handy rewrote the settings file from memory after they were deleted on disk. Delete them in Settings > Word Replacements, not in the file, while Handy runs.

Plain 29% (10/34): newterm 0/6, recurring 4/14 LOOSE (p12, p14, p16, p17 right first time thanks to learned pairs), recall 12/12.

### Eval bugs found and fixed

- **q7, q8, q9 were frozen too early.** The page waited for 2 option+space presses it never sees (Handy grabs the hotkey: 0 presses logged on every phrase), so it froze 12 s after the last character. When live typing stops at a "scratch that", the final text landed after the freeze. Handy did type it ("55 typed, 11 backspaces, 128 chars inserted"). The page now freezes once the dictation is in Handy's history plus 1 s of quiet.
- The key log was useless for the same reason: replaced by `recordings` per phrase, from the Handy log. Log windows started 2 s early and caught the previous phrase's dictation; fixed.

### Fixed in code

| Commit | Problem | Fix | Cases |
|---|---|---|---|
| `408085c` | "lunch on" -> LangChain, "call the" -> Claude, "Killashandra scored" -> "Killashandra harbor", "inventory in Databricks" -> "inventory Databricks" | Soundex boost only for edit ratio <= 0.4 (it codes 4 consonants, so it agreed on prefixes); an n-gram may not swallow a neighbour; learned phrases are never fuzzy names | p1, p9, p10, p26, q7, q15 |
| `9faed48` | q14 lost its first ~5 s in the app | `--stream --realtime` replays a WAV at mic speed (`ASR_REALTIME=1 eval.sh asr`) | diagnosis only |

`eval.sh asr` on 90 clips: **16 better, 0 worse** (including the effect of the 12 pairs learned during the run).

### Still open (new)

- **q14 (and nt4 in iteration 1): the first chunk of a dictation decoded to nothing in the app** ("Live preview perf: … committed_audio=3.12s, revision=0, 0 updates"). The saved WAV holds the words (it gets the same VAD frames as the stream) and `--realtime` replay keeps them 3/3. 1 dictation in 50; cause not found (engine first-chunk decode or VAD burst timing).
- **"no wait" is heard as "Now wait"** (q1, q4, q18): the custom prompt keeps both options. Check again with the backtrack prompt.
- p11: after a restarted dictation, live typing could not verify the field and copied the final text to the clipboard (safe fallback, 1 in 54).

## Next iteration (checklist)

1. In Handy: Settings > Post-processing, select **"Clean + resolve self-corrections"** (the custom prompt can be re-selected afterwards). In Settings > Word Replacements delete `test -> text`, `with pet -> we expect`, `forecasted -> forecast`.
2. Make sure the build under test is current (`bun run tauri dev` rebuilds on save; otherwise `cd src-tauri && cargo build`).
3. `eval.sh gen` only for a fresh set; for the pending retest keep the current `phrases.json` to compare with iteration 2.
4. `eval.sh serve plain`, then `eval.sh grade`; `eval.sh serve pp`, then `eval.sh grade`. Use Skip instead of restarting a phrase.
5. `eval.sh cases && eval.sh asr` (learning check on the same audio), `eval.sh trend`.
6. Compare with the iteration 1 rows above; look at `runs/<run>/logs/` for any phrase with lost words or restarts.

## Rules

- Change product code only (`src-tauri/`, and the prompt in `settings.rs`). Do not edit `phrases.json`, `expected`, or the graders to make cases pass; a grader bug fix gets its own note with the re-graded numbers.
- Follow `AGENTS.md`: `cargo fmt`, `cargo clippy`, tests, conventional commits, feature-freeze and PR-template rules. Do not push or open a PR without the user's say-so.
- Keep a before/after: `eval.sh trend` and `eval.sh asr` (and the failing case ids that now pass) in the commit or PR notes.
- `runs/`, `phrases.json` and `cases.jsonl` are local (gitignored); `runs/<run>/audio/` holds the WAVs, `runs/<run>/logs/` the Handy log lines. They live in the main checkout, so a worktree needs their absolute paths (`cases.jsonl` already stores absolute audio paths).

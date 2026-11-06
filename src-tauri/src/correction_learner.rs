//! Auto-learn: after a dictation is inserted, watch the focused field for a
//! short while and learn exact word replacements from the user's own fixes.
//!
//! `learn` and `upsert_learned` are pure and OS-agnostic; the watcher uses
//! `crate::accessibility`, which is a no-op off macOS. Field text stays in
//! memory only: only the learned pair is persisted.

use crate::accessibility;
use crate::actions::{disables_reasoning, post_process_llm, strip_think_block};
use crate::settings::{
    get_settings, write_settings, AppSettings, PostProcessProvider, WordReplacement,
    APPLE_INTELLIGENCE_PROVIDER_ID,
};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

const ANCHOR_CHARS: usize = 16;
const MAX_PHRASE_WORDS: usize = 3;
const MIN_LETTERS: usize = 3;
const MIN_SIMILARITY: f64 = 0.35;
const MAX_CHANGED_RATIO: f64 = 0.5;

const STOPWORDS: &[&str] = &[
    "the", "and", "that", "with", "for", "you", "this", "but", "his", "her", "she", "they", "them",
    "from", "have", "has", "had", "not", "are", "was", "were", "been", "will", "would", "can",
    "could", "should", "what", "when", "where", "which", "who", "why", "how", "all", "any", "some",
    "there", "their", "then", "than", "into", "about", "just", "your", "our", "out", "its", "also",
    "one",
];

/// Result of comparing the field before and after the user's edits.
#[derive(Debug, Default, PartialEq)]
pub struct LearnOutcome {
    /// Exact `(from, to)` replacements to learn.
    pub pairs: Vec<(String, String)>,
    /// Custom words (as spelled in the list) that the user corrected away:
    /// the fuzzy matcher misfired on them, so no pair is learned.
    pub misfires: Vec<String>,
}

/// Learns word corrections the user made to `inserted` inside a text field.
///
/// `before` is the field right after insertion, `after` the field once the
/// user is done. The inserted span must occur exactly once in `before`; up to
/// 16 chars on each side of it are anchors that must each occur exactly once
/// in `after` (an empty or blank anchor means the field edge), otherwise nothing is
/// learned. Edits outside the anchors are therefore ignored.
///
/// The span is word-aligned (LCS); a run of 1..=3 removed words followed by a
/// run of 1..=3 added words is a substitution. Aborts as a rewrite when more
/// than half of the inserted words were removed (so a one-word dictation is
/// never learned from). A substitution whose `from` contains one of
/// `custom_words` is reported in `misfires` instead of learned.
pub fn learn(inserted: &str, before: &str, after: &str, custom_words: &[String]) -> LearnOutcome {
    let mut outcome = LearnOutcome::default();
    let inserted = &fold_typography(inserted.trim());
    let (before, after) = (&fold_typography(before), &fold_typography(after));
    if inserted.is_empty() {
        return outcome;
    }
    let Some(start) = find_unique(before, inserted) else {
        return outcome;
    };
    // Whitespace alone (e.g. the trailing space the paste adds) is never
    // unique, so it counts as the field edge.
    fn anchor(s: &str) -> &str {
        if s.trim().is_empty() {
            ""
        } else {
            s
        }
    }
    let prefix = anchor(tail_chars(&before[..start], ANCHOR_CHARS));
    let suffix = anchor(head_chars(&before[start + inserted.len()..], ANCHOR_CHARS));

    let region_start = if prefix.is_empty() {
        0
    } else {
        match find_unique(after, prefix) {
            Some(p) => p + prefix.len(),
            None => return outcome,
        }
    };
    let rest = &after[region_start..];
    let region_end = if suffix.is_empty() {
        rest.len()
    } else {
        match find_unique(rest, suffix) {
            Some(p) => p,
            None => return outcome,
        }
    };
    let edited = &rest[..region_end];

    let old: Vec<&str> = inserted.split_whitespace().collect();
    let new: Vec<&str> = edited.split_whitespace().collect();
    let (substitutions, removed) = word_diff(&old, &new);
    if removed as f64 / old.len() as f64 > MAX_CHANGED_RATIO {
        return outcome;
    }

    for (from_words, to_words) in substitutions {
        if from_words.len() > MAX_PHRASE_WORDS || to_words.len() > MAX_PHRASE_WORDS {
            continue;
        }
        // At the field edge, words typed after fixing the last word join the
        // fix's run ("Andy" -> "Handy app"), so a growing tail run is unsafe.
        if suffix.is_empty()
            && to_words.len() > from_words.len()
            && old.ends_with(&from_words)
            && new.ends_with(&to_words)
        {
            continue;
        }
        let from = trim_punct(&from_words.join(" ")).to_string();
        let to = trim_punct(&to_words.join(" ")).to_string();
        let (from_key, to_key) = (normalize(&from), normalize(&to));
        if from_key.is_empty() || to_key.is_empty() || from_key == to_key {
            continue; // pure case or punctuation change
        }
        if let Some(word) = misfired_custom_word(&from_key, custom_words) {
            if !outcome.misfires.contains(&word) {
                outcome.misfires.push(word);
            }
            continue;
        }
        if letters(&from) < MIN_LETTERS || letters(&to) < MIN_LETTERS {
            continue;
        }
        if is_all_stopwords(&from_key) || is_all_stopwords(&to_key) {
            continue;
        }
        if strsim::normalized_levenshtein(&from.to_lowercase(), &to.to_lowercase()) < MIN_SIMILARITY
        {
            continue;
        }
        if !outcome
            .pairs
            .iter()
            .any(|(f, _)| f.eq_ignore_ascii_case(&from))
        {
            outcome.pairs.push((from, to));
        }
    }
    outcome
}

/// Adds or updates a learned pair, matching `from` case-insensitively.
/// Manual (non-learned) entries are never overwritten. Returns true if the
/// list changed.
pub fn upsert_learned(list: &mut Vec<WordReplacement>, from: &str, to: &str) -> bool {
    let key = from.to_lowercase();
    match list.iter_mut().find(|r| r.from.to_lowercase() == key) {
        Some(r) if r.learned && r.to != to => {
            r.to = to.to_string();
            r.last_used = crate::utils::unix_now();
            true
        }
        Some(_) => false,
        None => {
            list.push(WordReplacement {
                from: from.to_string(),
                to: to.to_string(),
                learned: true,
                last_used: crate::utils::unix_now(),
                ..Default::default()
            });
            true
        }
    }
}

/// Folds the substitutions apps apply to typed text (smart quotes, NBSP), so
/// the inserted transcript can still be found in the field.
pub(crate) fn fold_typography(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            '\u{00A0}' => ' ',
            c => c,
        })
        .collect()
}

/// Byte offset of the only occurrence of `needle` in `hay`, if exactly one.
fn find_unique(hay: &str, needle: &str) -> Option<usize> {
    let first = hay.find(needle)?;
    (hay.rfind(needle) == Some(first)).then_some(first)
}

fn tail_chars(s: &str, n: usize) -> &str {
    let start = s.char_indices().rev().nth(n - 1).map_or(0, |(i, _)| i);
    &s[start..]
}

fn head_chars(s: &str, n: usize) -> &str {
    let end = s.char_indices().nth(n).map_or(s.len(), |(i, _)| i);
    &s[..end]
}

type Substitution<'a> = (Vec<&'a str>, Vec<&'a str>);

/// Word-level LCS diff. Returns each (removed run, added run) where a removal
/// is directly followed by an addition, and the total number of removed words.
fn word_diff<'a>(old: &[&'a str], new: &[&'a str]) -> (Vec<Substitution<'a>>, usize) {
    let (n, m) = (old.len(), new.len());
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if old[i] == new[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    let mut removed_total = 0;
    let (mut removed, mut added) = (Vec::new(), Vec::new());
    let mut flush = |removed: &mut Vec<&'a str>, added: &mut Vec<&'a str>| {
        if !removed.is_empty() && !added.is_empty() {
            out.push((std::mem::take(removed), std::mem::take(added)));
        }
        removed.clear();
        added.clear();
    };
    while i < n || j < m {
        if i < n && j < m && old[i] == new[j] {
            flush(&mut removed, &mut added);
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
            if !added.is_empty() {
                flush(&mut removed, &mut added); // an add before a remove is not a substitution
            }
            removed.push(old[i]);
            removed_total += 1;
            i += 1;
        } else {
            added.push(new[j]);
            j += 1;
        }
    }
    flush(&mut removed, &mut added);
    (out, removed_total)
}

fn trim_punct(s: &str) -> &str {
    s.trim_matches(|c: char| !c.is_alphanumeric())
}

/// Lowercase alphanumeric words joined by single spaces.
fn normalize(s: &str) -> String {
    s.split_whitespace()
        .map(|w| trim_punct(w).to_lowercase())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn letters(s: &str) -> usize {
    s.chars().filter(|c| c.is_alphabetic()).count()
}

fn is_all_stopwords(key: &str) -> bool {
    key.split(' ').all(|w| STOPWORDS.contains(&w))
}

/// The custom word contained (as whole words) in the normalized `from`.
fn misfired_custom_word(from_key: &str, custom_words: &[String]) -> Option<String> {
    let padded = format!(" {from_key} ");
    custom_words
        .iter()
        .find(|cw| {
            let cw_key = normalize(cw);
            !cw_key.is_empty() && padded.contains(&format!(" {cw_key} "))
        })
        .cloned()
}

const GATE_MAX_CANDIDATES: usize = 4;
const GATE_TIMEOUT: Duration = Duration::from_secs(5);
const GATE_PROMPT: &str = "You check corrections a user made to text produced by speech recognition. \
Each line is `misheard -> corrected`. Accept a correction only when the corrected text is vocabulary \
a speech recognizer could not know: a name of a person, place or organization, a brand or product, \
an acronym, jargon or a technical term. Reject stylistic rewrites, synonyms, grammar or tense changes, \
fixes to ordinary words, and anything you are unsure about. Answer with JSON only, no other text: \
{\"accepted\": [...]}, listing each accepted corrected value exactly as written, or an empty list.";

/// The pairs sent to the LLM gate: at most 4, those whose `to` has an
/// uppercase letter (likely a name or brand) first. The rest are dropped.
fn gate_candidates(mut pairs: Vec<(String, String)>) -> Vec<(String, String)> {
    pairs.sort_by_key(|(_, to)| !to.chars().any(char::is_uppercase));
    pairs.truncate(GATE_MAX_CANDIDATES);
    pairs
}

/// One `from -> to` line per candidate: the only text sent to the LLM.
fn gate_user_content(candidates: &[(String, String)]) -> String {
    candidates
        .iter()
        .map(|(from, to)| format!("{from} -> {to}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The accepted `to` values in the LLM's reply that are exact candidates.
/// Tolerates a think block or code fence around the JSON object.
fn parse_gate_reply(reply: &str, candidates: &[(String, String)]) -> Result<Vec<String>, String> {
    #[derive(Deserialize)]
    struct Reply {
        accepted: Vec<String>,
    }
    let reply = strip_think_block(reply);
    let json = match (reply.find('{'), reply.rfind('}')) {
        (Some(start), Some(end)) if start < end => &reply[start..=end],
        _ => return Err("no JSON object in the reply".to_string()),
    };
    let reply: Reply = serde_json::from_str(json).map_err(|e| format!("invalid JSON: {e}"))?;
    Ok(reply
        .accepted
        .into_iter()
        .filter(|word| candidates.iter().any(|(_, to)| to == word))
        .collect())
}

/// Keeps only the pairs the LLM accepts as real vocabulary. `ask(system,
/// user)` makes the one request; any failure learns nothing (fail closed).
fn llm_gate(
    pairs: Vec<(String, String)>,
    ask: impl FnOnce(&str, &str) -> Result<String, String>,
) -> Vec<(String, String)> {
    let candidates = gate_candidates(pairs);
    let accepted = match ask(GATE_PROMPT, &gate_user_content(&candidates))
        .and_then(|reply| parse_gate_reply(&reply, &candidates))
    {
        Ok(accepted) => accepted,
        Err(err) => {
            warn!("Auto-learn: LLM filter failed, learning nothing: {err}");
            return Vec::new();
        }
    };
    let kept: Vec<_> = candidates
        .into_iter()
        .filter(|(_, to)| accepted.contains(to))
        .collect();
    debug!("Auto-learn: LLM filter kept {} pair(s)", kept.len());
    kept
}

/// The user's post-processing LLM, if one is configured and usable.
fn gate_llm(settings: &AppSettings) -> Option<(PostProcessProvider, String, String)> {
    let llm = post_process_llm(settings)?;
    if llm.0.id == APPLE_INTELLIGENCE_PROVIDER_ID {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        if !crate::apple_intelligence::check_apple_intelligence_availability() {
            return None;
        }
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        return None;
    }
    Some(llm)
}

/// Sends one gate request, mirroring how post-processing calls each kind of
/// provider, and gives up after `GATE_TIMEOUT`.
fn ask_llm(
    (provider, model, api_key): (PostProcessProvider, String, String),
    system: &str,
    user: &str,
) -> Result<String, String> {
    let (system, user) = (system.to_string(), user.to_string());
    let request = async move {
        if provider.id == APPLE_INTELLIGENCE_PROVIDER_ID {
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let token_limit = model.trim().parse::<i32>().unwrap_or(0);
                return tauri::async_runtime::spawn_blocking(move || {
                    crate::apple_intelligence::process_text_with_system_prompt(
                        &system,
                        &user,
                        token_limit,
                    )
                })
                .await
                .map_err(|e| e.to_string())?;
            }
            #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
            return Err("Apple Intelligence is unsupported here".to_string());
        }
        let disable_reasoning = disables_reasoning(&provider);
        if provider.supports_structured_output {
            let schema = serde_json::json!({
                "type": "object",
                "properties": {
                    "accepted": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["accepted"],
                "additionalProperties": false
            });
            match crate::llm_client::send_chat_completion_with_schema(
                &provider,
                api_key.clone(),
                &model,
                user.clone(),
                Some(system.clone()),
                Some(schema),
                disable_reasoning,
            )
            .await
            {
                Ok(reply) => return reply.ok_or_else(|| "empty response".to_string()),
                // Some endpoints reject the schema; post-processing falls back too.
                Err(err) => debug!("Auto-learn: structured output failed ({err}), retrying plain"),
            }
        }
        crate::llm_client::send_chat_completion(
            &provider,
            api_key,
            &model,
            format!("{system}\n\n{user}"),
            disable_reasoning,
        )
        .await?
        .ok_or_else(|| "empty response".to_string())
    };
    tauri::async_runtime::block_on(tokio::time::timeout(GATE_TIMEOUT, request))
        .map_err(|_| format!("no answer within {}s", GATE_TIMEOUT.as_secs()))?
}

static WATCH_GENERATION: AtomicU64 = AtomicU64::new(0);
const SETTLE_DELAY: Duration = Duration::from_millis(150);
const BASELINE_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const WATCH_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone, Serialize)]
struct WordLearnedEvent {
    from: String,
    to: String,
}

#[derive(Clone, Serialize)]
struct CustomWordMisfireEvent {
    word: String,
}

/// Stops any running correction watcher; called when a new dictation starts.
pub fn cancel_watchers() {
    WATCH_GENERATION.fetch_add(1, Ordering::SeqCst);
}

/// Watches the focused field for up to 60 s after `inserted` was put into it,
/// then learns the user's corrections. Returns immediately.
pub fn spawn_correction_watcher(app: &AppHandle, inserted: String) {
    let app = app.clone();
    // Captured before spawning so a dictation started meanwhile cancels us.
    let generation = WATCH_GENERATION.load(Ordering::SeqCst);
    std::thread::spawn(move || {
        let cancelled = || WATCH_GENERATION.load(Ordering::SeqCst) != generation;
        let needle = fold_typography(inserted.trim());
        if needle.is_empty() {
            return;
        }
        // Typed (non-clipboard) pastes may land after `paste` returns, and
        // Chrome/Electron build their AX tree only after AXManualAccessibility.
        let baseline_deadline = Instant::now() + BASELINE_TIMEOUT;
        let (field, before) = loop {
            std::thread::sleep(SETTLE_DELAY);
            if cancelled() {
                return;
            }
            if let Some(f) = accessibility::focused_field() {
                if f.is_secure() {
                    return;
                }
                accessibility::enable_manual_accessibility(f.pid());
                if let Some(v) = f.value().filter(|v| fold_typography(v).contains(&needle)) {
                    break (f, v);
                }
            }
            if Instant::now() >= baseline_deadline {
                debug!("Auto-learn: inserted text not found in focused field");
                return;
            }
        };

        let deadline = Instant::now() + WATCH_WINDOW;
        let mut last = before.clone();
        while Instant::now() < deadline {
            std::thread::sleep(POLL_INTERVAL);
            if cancelled() {
                break;
            }
            if field.is_secure() {
                return;
            }
            // Read before the focus check so a fix followed by a quick switch
            // is kept; skip empty reads (e.g. a chat box cleared on send).
            if let Some(v) = field.value().filter(|v| !v.trim().is_empty()) {
                last = v;
            }
            match accessibility::focused_field() {
                Some(f) if f.same_as(&field) => {}
                _ => break,
            }
        }

        let settings = get_settings(&app);
        if !settings.auto_learn_enabled {
            return;
        }
        let outcome = learn(&inserted, &before, &last, &settings.fuzzy_words());
        if outcome == LearnOutcome::default() {
            debug!(
                "Auto-learn: no correction to learn (field changed: {})",
                last != before
            );
        }
        let mut pairs = outcome.pairs;
        if settings.auto_learn_llm_filter && !pairs.is_empty() {
            match gate_llm(&settings) {
                Some(llm) => pairs = llm_gate(pairs, |system, user| ask_llm(llm, system, user)),
                None => debug!("Auto-learn: LLM filter on but no LLM configured"),
            }
        }
        // Re-read: the LLM call can take seconds, and the user may have
        // changed a setting meanwhile.
        let mut settings = get_settings(&app);
        let learned: Vec<_> = pairs
            .into_iter()
            .filter(|(from, to)| upsert_learned(&mut settings.word_replacements, from, to))
            .collect();
        if !learned.is_empty() {
            write_settings(&app, settings);
            let _ = app.emit(
                "settings-changed",
                serde_json::json!({ "setting": "word_replacements" }),
            );
        }
        for (from, to) in learned {
            info!(
                "Auto-learn: learned '{}' -> '{}'",
                crate::utils::redact_text(&from),
                crate::utils::redact_text(&to)
            );
            let _ = app.emit("word-learned", WordLearnedEvent { from, to });
        }
        for word in outcome.misfires {
            let _ = app.emit("custom-word-misfire", CustomWordMisfireEvent { word });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(inserted: &str, before: &str, after: &str) -> Vec<(String, String)> {
        learn(inserted, before, after, &[]).pairs
    }

    fn pair(from: &str, to: &str) -> Vec<(String, String)> {
        vec![(from.to_string(), to.to_string())]
    }

    #[test]
    fn learns_simple_typo_fix() {
        let ins = "I really like using Andy for dictation.";
        assert_eq!(
            pairs(ins, ins, "I really like using Handy for dictation."),
            pair("Andy", "Handy")
        );
    }

    #[test]
    fn learns_with_surrounding_text_and_anchors() {
        let before = "Dear team,\nI really like using Andy for dictation. Thanks!\nBob";
        let after = "Dear team,\nI really like using Handy for dictation. Thanks!\nBob";
        assert_eq!(
            pairs("I really like using Andy for dictation. ", before, after),
            pair("Andy", "Handy")
        );
    }

    // The paste appends a trailing space, so whitespace is all that follows.
    #[test]
    fn learns_with_trailing_space_after_insert() {
        let ins = "implement this new picture";
        assert_eq!(
            pairs(
                ins,
                "Notes: implement this new picture ",
                "Notes: implement this new feature "
            ),
            pair("picture", "feature")
        );
        assert_eq!(
            pairs(
                ins,
                "\n implement this new picture ",
                "\n implement this new feature "
            ),
            pair("picture", "feature")
        );
    }

    // At the field edge, words typed after the fix join its run.
    #[test]
    fn ignores_words_typed_after_a_fix_at_the_field_edge() {
        let ins = "I like Andy";
        assert!(pairs(ins, "I like Andy ", "I like Handy app ").is_empty());
        assert_eq!(
            pairs(ins, "I like Andy ", "I like Handy "),
            pair("Andy", "Handy")
        );
    }

    #[test]
    fn learns_multi_word_to_single_word() {
        let ins = "we should try lang chain for the agent pipeline today";
        assert_eq!(
            pairs(
                ins,
                ins,
                "we should try LangChain for the agent pipeline today"
            ),
            pair("lang chain", "LangChain")
        );
    }

    #[test]
    fn rejects_rewrite() {
        let ins = "send the report to marketing tomorrow";
        assert!(pairs(ins, ins, "please forward this document to sales later").is_empty());
        // 3 of 5 words changed: each change looks like a typo fix, but it's a rewrite.
        let ins = "send Andy Codex Klaude now";
        assert!(pairs(ins, ins, "send Handy Codecs Claude now").is_empty());
    }

    #[test]
    fn rejects_unrelated_edit() {
        let ins = "I will bring the cake to the party tonight";
        assert!(pairs(ins, ins, "I will bring the wine to the party tonight").is_empty());
    }

    #[test]
    fn aborts_on_ambiguous_anchor() {
        // The 16 chars before the span occur twice in `after`.
        let prefix = "Notes for today: ";
        let before = format!("{prefix}I like using Andy for dictation now");
        let after = format!("{prefix}{prefix}I like using Handy for dictation now");
        assert!(pairs("I like using Andy for dictation now", &before, &after).is_empty());
    }

    #[test]
    fn aborts_when_anchor_missing() {
        let before = "Header line here. I like using Andy for dictation now. Footer";
        let after = "Changed it all!! I like using Handy for dictation now. Footer";
        assert!(pairs("I like using Andy for dictation now.", before, after).is_empty());
    }

    #[test]
    fn aborts_when_inserted_not_unique_in_before() {
        let ins = "using Andy daily";
        let before = format!("{ins} and {ins}");
        assert!(pairs(ins, &before, "using Handy daily and using Andy daily").is_empty());
    }

    #[test]
    fn ignores_edit_outside_span() {
        // The typo fix is well before the 16-char prefix anchor, which survives.
        let before = "Grocery list: bananna, milk. Remember the eggs too.\n\nI like using Andy now";
        let after = "Grocery list: banana, milk. Remember the eggs too.\n\nI like using Andy now";
        assert!(pairs("I like using Andy now", before, after).is_empty());
    }

    #[test]
    fn appended_text_after_span_learns_nothing() {
        let ins = "I like using Andy for dictation now";
        let after = format!("{ins} and then some more typing");
        assert!(pairs(ins, ins, &after).is_empty());
    }

    #[test]
    fn custom_word_misfire_gives_notice_not_pair() {
        let ins = "I came Frontend store yesterday";
        let out = learn(
            ins,
            ins,
            "I came from the store yesterday",
            &["Frontend".to_string()],
        );
        assert!(out.pairs.is_empty());
        assert_eq!(out.misfires, vec!["Frontend".to_string()]);
    }

    #[test]
    fn custom_word_check_is_whole_word() {
        let ins = "please check the Andyx module now";
        let out = learn(
            ins,
            ins,
            "please check the Handyx module now",
            &["and".to_string()],
        );
        assert_eq!(out.pairs, pair("Andyx", "Handyx"));
        assert!(out.misfires.is_empty());
    }

    #[test]
    fn skips_case_and_punctuation_only_changes() {
        let ins = "I like using handy for dictation now";
        assert!(pairs(ins, ins, "I like using Handy for dictation now").is_empty());
        assert!(pairs(ins, ins, "I like using handy, for dictation now").is_empty());
    }

    #[test]
    fn skips_short_words_and_stopwords() {
        let ins = "I want to go to the shop now please";
        assert!(pairs(ins, ins, "I want to go in the shop now please").is_empty());
        let ins = "they said that then it was fine";
        assert!(pairs(ins, ins, "they said than then it was fine").is_empty());
    }

    #[test]
    fn non_ascii_inside_anchors() {
        // Accents and emoji within the 16-char anchor zones on both sides.
        let before = "Café ☕ résumé 🎉 I like using Andy for dictation 😀 naïve über";
        let after = "Café ☕ résumé 🎉 I like using Handy for dictation 😀 naïve über";
        assert_eq!(
            pairs("I like using Andy for dictation", before, after),
            pair("Andy", "Handy")
        );
    }

    #[test]
    fn smart_quotes_and_nbsp_in_field_still_match() {
        let ins = "I don't like using Andy for dictation";
        let before = "I don\u{2019}t like\u{00A0}using Andy for dictation";
        let after = "I don\u{2019}t like\u{00A0}using Handy for dictation";
        assert_eq!(pairs(ins, before, after), pair("Andy", "Handy"));
    }

    #[test]
    fn non_ascii_words_are_learned() {
        let ins = "abbiamo mangiato al caffe del centro ieri";
        assert_eq!(
            pairs(ins, ins, "abbiamo mangiato al caffè del centro ieri"),
            pair("caffe", "caffè")
        );
    }

    #[test]
    fn upsert_adds_updates_learned_and_keeps_manual() {
        let mut list = vec![WordReplacement {
            from: "Gpt".into(),
            to: "GPT".into(),
            learned: false,
            ..Default::default()
        }];
        assert!(upsert_learned(&mut list, "andy", "Handy"));
        assert_eq!(list.len(), 2);
        assert!(list[1].learned);

        assert!(upsert_learned(&mut list, "Andy", "HANDY"));
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].to, "HANDY");
        assert!(!upsert_learned(&mut list, "ANDY", "HANDY"));

        assert!(!upsert_learned(&mut list, "gpt", "G.P.T."));
        assert_eq!(list[0].to, "GPT");
        assert_eq!(list.len(), 2);
    }

    fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(f, t)| (f.to_string(), t.to_string()))
            .collect()
    }

    #[test]
    fn gate_candidates_rank_capitalized_first_and_cap_at_four() {
        let pairs = owned(&[
            ("colour", "color"),
            ("andy", "Handy"),
            ("recieve", "receive"),
            ("lang chain", "LangChain"),
            ("teh", "tea"),
            ("kuber", "k8s"),
            ("open ai", "OpenAI"),
        ]);
        assert_eq!(
            gate_candidates(pairs),
            owned(&[
                ("andy", "Handy"),
                ("lang chain", "LangChain"),
                ("open ai", "OpenAI"),
                ("colour", "color"),
            ])
        );
    }

    #[test]
    fn gate_prompt_holds_only_the_candidate_pairs() {
        let candidates = owned(&[("andy", "Handy"), ("lang chain", "LangChain")]);
        assert_eq!(
            gate_user_content(&candidates),
            "andy -> Handy\nlang chain -> LangChain"
        );
        assert!(GATE_PROMPT.contains("\"accepted\""));
    }

    #[test]
    fn gate_reply_parsing() {
        let candidates = owned(&[("andy", "Handy"), ("colour", "color")]);
        let parse = |reply: &str| parse_gate_reply(reply, &candidates);
        assert_eq!(
            parse(r#"{"accepted": ["Handy"]}"#),
            Ok(vec!["Handy".into()])
        );
        assert_eq!(parse(r#"{"accepted": []}"#), Ok(vec![]));
        assert_eq!(
            parse("<think>hmm</think>```json\n{\"accepted\": [\"Handy\"]}\n```"),
            Ok(vec!["Handy".into()])
        );
        // Only exact candidate `to` values count.
        assert_eq!(
            parse(r#"{"accepted": ["handy", "andy", "Kubernetes", "Handy"]}"#),
            Ok(vec!["Handy".into()])
        );
        assert!(parse("Handy").is_err());
        assert!(parse(r#"{"accepted": "Handy"}"#).is_err());
        assert!(parse(r#"{"words": ["Handy"]}"#).is_err());
        assert!(parse(r#"{"accepted": ["Handy"]"#).is_err());
    }

    #[test]
    fn llm_gate_keeps_accepted_and_fails_closed() {
        let pairs = owned(&[("andy", "Handy"), ("colour", "color")]);
        let kept = llm_gate(pairs.clone(), |system, user| {
            assert_eq!(system, GATE_PROMPT);
            assert_eq!(user, "andy -> Handy\ncolour -> color");
            Ok(r#"{"accepted": ["Handy"]}"#.into())
        });
        assert_eq!(kept, owned(&[("andy", "Handy")]));

        assert!(llm_gate(pairs.clone(), |_, _| Err("HTTP 401".into())).is_empty());
        assert!(llm_gate(pairs.clone(), |_, _| Ok("Sure! Handy".into())).is_empty());
        assert!(llm_gate(pairs, |_, _| Ok(r#"{"accepted": ["Kubernetes"]}"#.into())).is_empty());
    }

    #[test]
    fn llm_gate_drops_pairs_past_the_cap() {
        let pairs = owned(&[
            ("a1", "Alpha"),
            ("b1", "Bravo"),
            ("c1", "Charlie"),
            ("d1", "Delta"),
            ("e1", "Echo"),
        ]);
        let kept = llm_gate(pairs, |_, user| {
            assert!(!user.contains("Echo"));
            Ok(r#"{"accepted": ["Alpha", "Bravo", "Charlie", "Delta", "Echo"]}"#.into())
        });
        assert_eq!(kept.len(), 4);
        assert!(!kept.iter().any(|(_, to)| to == "Echo"));
    }

    #[test]
    fn anchor_helpers_count_chars_not_bytes() {
        assert_eq!(tail_chars("ab😀é", 2), "😀é");
        assert_eq!(head_chars("é😀ab", 2), "é😀");
        assert_eq!(tail_chars("ab", 16), "ab");
        assert_eq!(head_chars("", 16), "");
    }
}

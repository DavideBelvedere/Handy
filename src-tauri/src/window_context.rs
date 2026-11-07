//! Names and terms visible around the cursor in the focused app, read when a
//! dictation starts and sent to the model as extra vocabulary, so a name on
//! screen comes out right the first time. Opt-in (`window_context_vocabulary`)
//! and macOS/Windows only: elsewhere the Accessibility read finds no field.
//!
//! Only the focused field is read, not the window title or other elements:
//! titles are Title Case app chrome ("Untitled Document — Edited") that the
//! capital-letter heuristic can't tell from names, and walking the AX tree
//! has no cheap bound.
//!
//! Privacy: the field text is dropped once its terms are extracted. The terms
//! live only in this dictation's capture, are never persisted or logged above
//! `trace`, and go nowhere but the local model.

use crate::accessibility;
use log::{debug, trace};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Longest the read may take, from dictation start. A later read is
/// discarded: the dictation runs without context rather than wait for it.
const BUDGET: Duration = Duration::from_millis(50);

// shortcut: a fixed count; these go after custom and learned words, so a model over its token budget drops them first. Budget by tokens if that drops too many.
const MAX_TERMS: usize = 20;

/// Capitalized words common enough to be noise when not at a sentence start
/// (titles, UI labels, greetings).
const STOPWORDS: &[&str] = &[
    "the",
    "this",
    "that",
    "these",
    "those",
    "and",
    "but",
    "for",
    "with",
    "from",
    "you",
    "your",
    "our",
    "his",
    "her",
    "they",
    "them",
    "then",
    "there",
    "what",
    "when",
    "where",
    "which",
    "who",
    "why",
    "how",
    "not",
    "yes",
    "all",
    "any",
    "can",
    "will",
    "are",
    "was",
    "were",
    "has",
    "have",
    "had",
    "its",
    "also",
    "just",
    "please",
    "thanks",
    "thank",
    "hello",
    "dear",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
    "january",
    "february",
    "march",
    "april",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
    "che",
    "non",
    "per",
    "con",
    "del",
    "della",
    "dei",
    "delle",
    "nel",
    "nella",
    "sono",
    "come",
    "anche",
    "questo",
    "questa",
    "una",
    "uno",
    "gli",
    "ciao",
    "grazie",
    "buongiorno",
    "gentile",
];

/// One dictation's on-screen terms, read on a thread of their own.
pub struct WindowContext {
    deadline: Instant,
    state: Mutex<State>,
}

enum State {
    Reading(mpsc::Receiver<Vec<String>>),
    Read(Vec<String>),
}

impl WindowContext {
    /// Starts reading the focused field. Terms in `known` (custom words,
    /// learned targets) are already sent to the model, so they are skipped.
    pub fn capture(known: Vec<String>) -> Arc<Self> {
        let started = Instant::now();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let terms = read_terms(&known);
            let elapsed = started.elapsed();
            debug!(
                "Window context: {} terms read in {:?} (budget {:?})",
                terms.len(),
                elapsed,
                BUDGET
            );
            trace!("Window context terms: {terms:?}");
            if elapsed <= BUDGET {
                let _ = tx.send(terms);
            }
        });
        Arc::new(Self {
            deadline: started + BUDGET,
            state: Mutex::new(State::Reading(rx)),
        })
    }

    /// The terms, waiting at most until the budget ends; empty when the read
    /// missed it or found nothing.
    pub fn terms(&self) -> Vec<String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let State::Reading(rx) = &*state {
            let wait = self.deadline.saturating_duration_since(Instant::now());
            let terms = rx.recv_timeout(wait).unwrap_or_else(|_| {
                debug!("Window context: no terms within the budget");
                Vec::new()
            });
            *state = State::Read(terms);
        }
        match &*state {
            State::Read(terms) => terms.clone(),
            State::Reading(_) => unreachable!("read above"),
        }
    }
}

fn read_terms(known: &[String]) -> Vec<String> {
    // No wait for the tree: a Chrome/Electron app gives nothing on the first
    // dictation after it gains focus, and is readable on later ones.
    accessibility::enable_manual_accessibility_for_focused_app();
    accessibility::focused_field()
        .filter(|f| !f.is_secure())
        .and_then(|f| f.text_near_cursor())
        .map(|(text, cursor)| extract_terms(&text, cursor, known))
        .unwrap_or_default()
}

/// Likely names and jargon in `text`, nearest to `cursor` (a UTF-16 offset)
/// first: capitalized words not at a sentence start, and words with an inner
/// capital (CamelCase, ALL-CAPS acronyms) or a digit. Words under 3
/// characters, stopwords and `known` terms are dropped, so are 2-letter
/// acronyms; deduped ignoring case; at most [`MAX_TERMS`].
pub fn extract_terms(text: &str, cursor: usize, known: &[String]) -> Vec<String> {
    let known: Vec<String> = known.iter().map(|k| k.trim().to_lowercase()).collect();
    // (term, lowercase, distance from the cursor)
    let mut found: Vec<(String, String, usize)> = Vec::new();
    let mut sentence_start = true;
    let mut word = String::new();
    let mut word_start = 0usize;
    let mut offset = 0usize;
    // Apostrophes and hyphens split words: "dell'Agenzia" -> "Agenzia".
    for c in text.chars().chain([' ']) {
        if c.is_alphanumeric() {
            if word.is_empty() {
                word_start = offset;
            }
            word.push(c);
        } else if !word.is_empty() {
            if is_term(&word, sentence_start) {
                let lower = word.to_lowercase();
                let distance = word_start.abs_diff(cursor);
                match found.iter_mut().find(|f| f.1 == lower) {
                    Some(f) => f.2 = f.2.min(distance),
                    None => found.push((word.clone(), lower, distance)),
                }
            }
            sentence_start = false;
            word.clear();
        }
        if matches!(c, '.' | '!' | '?' | '\n' | '\r') {
            sentence_start = true;
        }
        offset += c.len_utf16();
    }
    found.retain(|f| !known.contains(&f.1));
    found.sort_by_key(|f| f.2);
    found.into_iter().take(MAX_TERMS).map(|f| f.0).collect()
}

fn is_term(word: &str, sentence_start: bool) -> bool {
    if word.chars().count() < 3
        || !word.chars().any(char::is_alphabetic)
        || STOPWORDS.contains(&word.to_lowercase().as_str())
    {
        return false;
    }
    let first_upper = word.chars().next().is_some_and(char::is_uppercase);
    word.chars().skip(1).any(char::is_uppercase)
        || word.chars().any(char::is_numeric)
        || (first_upper && !sentence_start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(text: &str) -> Vec<String> {
        extract_terms(text, 0, &[])
    }

    #[test]
    fn keeps_names_camel_case_acronyms_and_words_with_digits() {
        let mut got = terms("we met Davide at Revo. He likes PostgreSQL, GPT4 and the NASA API");
        got.sort();
        assert_eq!(got, ["API", "Davide", "GPT4", "NASA", "PostgreSQL", "Revo"]);
    }

    #[test]
    fn drops_capitals_at_a_sentence_start() {
        assert!(terms("Tomorrow we ship. Today we test!\nMaybe later").is_empty());
        // A name at a sentence start can't be told from any other word.
        assert_eq!(terms("Mario called. ask Mario"), ["Mario"]);
    }

    #[test]
    fn drops_short_words_stopwords_and_plain_words() {
        // "AI" is an acronym but under 3 letters; "The" a stopword; numbers
        // and lowercase words carry nothing.
        assert!(terms("ask AI about The plan on 2026 ok").is_empty());
        assert_eq!(terms("ask about Ollama"), ["Ollama"]);
    }

    #[test]
    fn splits_on_apostrophes_and_hyphens() {
        let mut got = terms("go to dell'Agenzia with Jean-Luc");
        got.sort();
        assert_eq!(got, ["Agenzia", "Jean", "Luc"]);
    }

    #[test]
    fn dedupes_ignoring_case_including_accents() {
        assert_eq!(terms("x Niccolò and NICCOLÒ and Niccolò"), ["Niccolò"]);
    }

    #[test]
    fn skips_custom_words_and_learned_targets() {
        let known = ["handy".to_string(), " Vandermeulen ".to_string()];
        assert_eq!(
            extract_terms("x Handy and Vandermeulen meet Okonkwo", 0, &known),
            ["Okonkwo"]
        );
    }

    #[test]
    fn caps_the_count_keeping_terms_nearest_the_cursor() {
        let text: String = (0..30).map(|i| format!("x Term{i:02} ")).collect();
        let cursor = text.encode_utf16().count();
        let got = extract_terms(&text, cursor, &[]);
        assert_eq!(got.len(), MAX_TERMS);
        assert_eq!(got[0], "Term29");
        assert_eq!(got[MAX_TERMS - 1], "Term10");
    }

    #[test]
    fn cursor_offset_is_in_utf16_units() {
        // Each emoji is 2 UTF-16 units, so "Bbb" starts at 12 (9 in chars):
        // from 6, "Aaa" at 2 is nearer.
        assert_eq!(extract_terms("x Aaa😀😀😀 Bbb", 6, &[]), ["Aaa", "Bbb"]);
    }

    #[test]
    fn a_read_past_the_deadline_gives_no_terms() {
        let (tx, rx) = mpsc::channel();
        let late = WindowContext {
            deadline: Instant::now(),
            state: Mutex::new(State::Reading(rx)),
        };
        assert!(late.terms().is_empty());
        // Once decided, a late result can't change it.
        let _ = tx.send(vec!["Late".to_string()]);
        assert!(late.terms().is_empty());
    }
}

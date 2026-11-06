//! Live typing: while a streaming dictation runs, types its stable prefix into
//! the focused field, then fixes the field up at the end with a verified
//! replace of the part that differs from the final text. Text is only ever deleted after Accessibility confirms it is still
//! the text we typed, right before the cursor, in the same field.
//!
//! Without an Accessibility target (non-macOS, unreadable field) no session
//! starts and the caller does the normal paste.

use crate::accessibility::{self, FocusedField, Utf16Range};
use crate::audio_toolkit::OutputLanguageEvidence;
use crate::clipboard::{send_return_key, with_enigo, write_text_to_clipboard};
use crate::correction_learner::fold_typography;
use crate::input;
use crate::managers::transcription::post_process_transcription_text;
use crate::settings::{get_settings, AppSettings, ClipboardHandling};
use enigo::{Direction, Key, Keyboard};
use log::{debug, warn};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use unicode_segmentation::UnicodeSegmentation;

/// Trailing words never typed live: custom-word n-grams reach 3 words, so a
/// later word can still rewrite them.
const HOLDBACK_WORDS: usize = 3;
/// The target app may not have applied the last keystrokes when we read it.
const VERIFY_ATTEMPTS: usize = 5;
const VERIFY_RETRY: Duration = Duration::from_millis(60);

enum Cmd {
    /// `generation` 0 = not live.
    Begin {
        generation: u64,
    },
    Committed {
        generation: u64,
        text: String,
        language: OutputLanguageEvidence,
        languages: Vec<String>,
    },
    Finish(String, mpsc::Sender<Finish>),
    Cancel,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Finish {
    /// Live typing delivered the output (typed it, or copied it to the
    /// clipboard when the field could not be verified).
    Handled,
    /// Nothing was typed live: do the normal paste.
    Fallback,
}

/// Handle to the typer thread. Every command travels on one ordered channel,
/// so all queued deltas are typed before a finish or cancel decision runs.
#[derive(Clone)]
pub struct LiveTyper {
    tx: mpsc::Sender<Cmd>,
    /// Generation of the live session, 0 when none. A stream captures it at
    /// start, so text from a cancelled stream never reaches the next session.
    current: Arc<AtomicU64>,
    last: Arc<AtomicU64>,
}

impl LiveTyper {
    pub fn new(app: AppHandle) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || run(app, rx));
        Self {
            tx,
            current: Arc::new(AtomicU64::new(0)),
            last: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Called at every dictation start; replaces any session an earlier
    /// dictation left behind. Never blocks: Accessibility work runs on the
    /// typer thread.
    pub fn begin(&self, live: bool) {
        let generation = if live {
            self.last.fetch_add(1, Ordering::AcqRel) + 1
        } else {
            0
        };
        self.current.store(generation, Ordering::Release);
        let _ = self.tx.send(Cmd::Begin { generation });
    }

    /// The live session's generation (0 = none), for a stream to capture at start.
    pub fn generation(&self) -> u64 {
        self.current.load(Ordering::Acquire)
    }

    /// Forwards the stream's raw committed text. Never blocks the engine thread.
    pub fn on_committed(
        &self,
        generation: u64,
        committed: &str,
        language: &OutputLanguageEvidence,
        languages: &[String],
    ) {
        if generation != 0 && generation == self.generation() {
            let _ = self.tx.send(Cmd::Committed {
                generation,
                text: committed.to_string(),
                language: language.clone(),
                languages: languages.to_vec(),
            });
        }
    }

    /// Delivers the final text. Blocks until the typer is done, so it must not
    /// run on the main thread (the typer types through it).
    pub fn finish(&self, final_text: String) -> Finish {
        self.current.store(0, Ordering::Release);
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::Finish(final_text, reply)).is_err() {
            return Finish::Fallback;
        }
        rx.recv().unwrap_or(Finish::Fallback)
    }

    /// Removes the typed text after verifying it. Never blocks.
    pub fn cancel(&self) {
        self.current.store(0, Ordering::Release);
        let _ = self.tx.send(Cmd::Cancel);
    }
}

struct Session {
    generation: u64,
    target: FocusedField,
    typed: String,
    /// Still typing deltas; cleared when the text diverges or focus moves.
    active: bool,
    /// A typing call failed, so the field content is unknown: never touch it again.
    poisoned: bool,
    settings: AppSettings,
}

fn run(app: AppHandle, rx: mpsc::Receiver<Cmd>) {
    let mut session: Option<Session> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Begin { generation } => session = begin_session(&app, generation),
            Cmd::Committed {
                generation,
                text,
                language,
                languages,
            } => {
                // A cancelled stream's late results belong to an older session.
                if let Some(s) = session
                    .as_mut()
                    .filter(|s| s.active && s.generation == generation)
                {
                    type_committed(&app, s, text, &language, &languages);
                }
            }
            Cmd::Finish(final_text, reply) => {
                let outcome = match session.take() {
                    Some(s) => finish_session(&app, s, &final_text),
                    None => Finish::Fallback,
                };
                let _ = reply.send(outcome);
            }
            Cmd::Cancel => {
                if let Some(s) = session.take() {
                    cancel_session(&app, s);
                }
            }
        }
    }
}

fn begin_session(app: &AppHandle, generation: u64) -> Option<Session> {
    let settings = get_settings(app);
    if generation != 0 || settings.auto_learn_enabled {
        accessibility::enable_manual_accessibility_for_focused_app();
    }
    if generation == 0 {
        return None;
    }
    // Chrome/Electron build their AX tree asynchronously after the flag.
    thread::sleep(Duration::from_millis(200));
    let Some(target) = accessibility::focused_field()
        .filter(|f| !f.is_secure() && is_text_target(f.role().as_deref()))
    else {
        debug!("Live typing: no non-secure focused text field; using normal paste");
        return None;
    };
    Some(Session {
        generation,
        target,
        typed: String::new(),
        active: true,
        poisoned: false,
        settings,
    })
}

fn type_committed(
    app: &AppHandle,
    s: &mut Session,
    raw: String,
    language: &OutputLanguageEvidence,
    languages: &[String],
) {
    let cleaned = post_process_transcription_text(raw, &s.settings, &[], language, languages);
    let delta = match next_delta(&s.typed, stable_prefix(&cleaned, HOLDBACK_WORDS)) {
        Delta::Nothing => return,
        Delta::Diverged => {
            debug!(
                "Live typing: cleaned text diverged after {} typed chars; stopping",
                s.typed.chars().count()
            );
            s.active = false;
            return;
        }
        Delta::Append(delta) => delta,
    };
    if !on_target(&s.target) {
        debug!("Live typing: focus moved; stopping");
        s.active = false;
        return;
    }
    match type_on_main(app, 0, &delta) {
        Ok(()) => s.typed.push_str(&delta),
        Err(e) => {
            warn!("Live typing: typing failed ({e}); stopping");
            s.active = false;
            s.poisoned = true;
        }
    }
}

fn finish_session(app: &AppHandle, s: Session, final_text: &str) -> Finish {
    let mut text = final_text.to_string();
    if s.settings.append_trailing_space && !text.is_empty() {
        text.push(' ');
    }
    let (backspaces, insert) = match plan_finish(&s.typed, &text) {
        FinishPlan::Fallback => return Finish::Fallback,
        FinishPlan::TypeRest(rest) => (Some(0), rest),
        FinishPlan::Replace { keep } => {
            (backspace_count(&s.typed[keep..]), text[keep..].to_string())
        }
    };
    // An untrusted backspace count is treated like a mismatch: never delete.
    let field = match backspaces {
        // Nothing left to type: the field already holds the whole output.
        Some(0) if insert.is_empty() && !s.poisoned => FieldCheck::Verified,
        Some(_) if !s.poisoned => verify_field(&s.target, &s.typed),
        _ => FieldCheck::Mismatch,
    };
    let backspaces = backspaces.unwrap_or(0);
    // Appending deletes nothing, so an unreadable field only needs focus;
    // deleting needs a verified read.
    let may_type =
        field == FieldCheck::Verified || (backspaces == 0 && field == FieldCheck::Unreadable);
    if may_type && type_on_main(app, backspaces, &insert).is_ok() {
        debug!(
            "Live typing finished: {} typed, {} backspaces, {} chars inserted",
            s.typed.chars().count(),
            backspaces,
            insert.chars().count()
        );
        finish_like_paste(app, &s.settings, text);
    } else if !final_text.is_empty() {
        warn!(
            "Live typing: field not verified after {} typed chars; final text copied to clipboard",
            s.typed.chars().count()
        );
        let final_text = final_text.to_string();
        if let Err(e) = on_main(app, move |app| write_text_to_clipboard(app, &final_text)) {
            warn!("Live typing: clipboard fallback failed: {e}");
        }
        let _ = app.emit("live-typing-fallback", ());
    }
    Finish::Handled
}

/// The parts of `clipboard::paste` that follow the insertion.
fn finish_like_paste(app: &AppHandle, settings: &AppSettings, text: String) {
    if settings.auto_submit {
        thread::sleep(Duration::from_millis(50));
        let key = settings.auto_submit_key;
        if let Err(e) = on_main(app, move |app| {
            with_enigo(app, move |enigo| send_return_key(enigo, key))
        }) {
            warn!("Live typing: auto-submit failed: {e}");
        }
    }
    if settings.clipboard_handling == ClipboardHandling::CopyToClipboard {
        if let Err(e) = on_main(app, move |app| write_text_to_clipboard(app, &text)) {
            warn!("Live typing: copy to clipboard failed: {e}");
        }
    }
}

fn cancel_session(app: &AppHandle, s: Session) {
    if s.typed.is_empty() {
        return;
    }
    let count = s.typed.chars().count();
    if !s.poisoned
        && backspace_count(&s.typed).is_some()
        && verify_field(&s.target, &s.typed) == FieldCheck::Verified
        && type_on_main(app, count, "").is_ok()
    {
        debug!("Live typing cancelled: removed {count} typed chars");
    } else {
        warn!("Live typing cancelled: field not verified, left {count} typed chars in place");
    }
}

fn on_target(target: &FocusedField) -> bool {
    accessibility::focused_field().is_some_and(|f| f.same_as(target))
}

#[derive(Debug, PartialEq, Eq)]
enum FieldCheck {
    /// The target has focus and holds `typed` right before the cursor, with
    /// nothing selected.
    Verified,
    /// Focus moved, or the field was read and never matched.
    Mismatch,
    /// The target has focus but its text could not be read.
    Unreadable,
}

fn verify_field(target: &FocusedField, typed: &str) -> FieldCheck {
    let len = typed.encode_utf16().count();
    let mut last_read = None;
    for attempt in 0..VERIFY_ATTEMPTS {
        if !on_target(target) {
            debug!("Live typing: verify failed, focus moved");
            return FieldCheck::Mismatch;
        }
        if let (Some(before), Some(selected)) =
            (target.text_before_cursor(len), target.selected_range())
        {
            if can_replace(&before, selected, typed) {
                return FieldCheck::Verified;
            }
            last_read = Some((before, selected));
        }
        if attempt + 1 < VERIFY_ATTEMPTS {
            thread::sleep(VERIFY_RETRY);
        }
    }
    match last_read {
        Some((before, selected)) => {
            debug!(
                "Live typing: verify failed, {}",
                mismatch_summary(&before, selected, typed)
            );
            FieldCheck::Mismatch
        }
        None => {
            debug!("Live typing: verify failed, field unreadable");
            FieldCheck::Unreadable
        }
    }
}

/// Why `before` failed [`can_replace`], in numbers and character classes only:
/// field text must never reach the log.
fn mismatch_summary(before: &str, selected: Utf16Range, typed: &str) -> String {
    let units = |s: &str| fold_typography(s).encode_utf16().collect::<Vec<u16>>();
    let (field, ours) = (units(before), units(typed));
    let same = field
        .iter()
        .rev()
        .zip(ours.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let class = |u: Option<&u16>| match u.map(|&u| char::from_u32(u.into())) {
        None => "none",
        Some(Some('\n' | '\r' | '\u{2028}' | '\u{2029}')) => "newline",
        Some(Some(c)) if c.is_whitespace() => "space",
        Some(Some(c)) if c.is_alphanumeric() => "alnum",
        Some(Some(c)) if c.is_ascii_punctuation() => "punct",
        _ => "other",
    };
    format!(
        "read {} of {} UTF-16 units, {} match back from the cursor, then field={} typed={}, selection length {}",
        field.len(),
        ours.len(),
        same,
        class(field.iter().rev().nth(same)),
        class(ours.iter().rev().nth(same)),
        selected.length
    )
}

/// Sends `backspaces` Backspace clicks, then types `text`, on the main thread
/// (where enigo must run), waiting for it so `typed` stays exact.
fn type_on_main(app: &AppHandle, backspaces: usize, text: &str) -> Result<(), String> {
    if backspaces == 0 && text.is_empty() {
        return Ok(());
    }
    let text = text.to_string();
    on_main(app, move |app| {
        with_enigo(app, move |enigo| {
            for _ in 0..backspaces {
                enigo
                    .key(Key::Backspace, Direction::Click)
                    .map_err(|e| format!("Failed to send Backspace: {e}"))?;
            }
            if text.is_empty() {
                Ok(())
            } else {
                input::paste_text_direct(enigo, &text)
            }
        })
    })
}

fn on_main<T: Send + 'static>(
    app: &AppHandle,
    f: impl FnOnce(&AppHandle) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = mpsc::channel();
    let handle = app.clone();
    app.run_on_main_thread(move || {
        let _ = tx.send(f(&handle));
    })
    .map_err(|e| format!("Failed to run on main thread: {e}"))?;
    rx.recv()
        .map_err(|_| "Main thread dropped the call".to_string())?
}

/// `cleaned` minus its last `holdback_words` words (and the whitespace before
/// them). `holdback_words` must be at least 1.
fn stable_prefix(cleaned: &str, holdback_words: usize) -> &str {
    let mut starts = Vec::new();
    let mut prev_ws = true;
    for (i, c) in cleaned.char_indices() {
        if !c.is_whitespace() && prev_ws {
            starts.push(i);
        }
        prev_ws = c.is_whitespace();
    }
    match starts.len().checked_sub(holdback_words) {
        Some(n) if n > 0 => cleaned[..starts[n]].trim_end(),
        _ => "",
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Delta {
    Append(String),
    Diverged,
    Nothing,
}

fn next_delta(typed: &str, stable: &str) -> Delta {
    match stable.strip_prefix(typed) {
        Some("") => Delta::Nothing,
        Some(rest) => Delta::Append(rest.to_string()),
        // Cleanup briefly shortened the text: wait for more.
        None if typed.starts_with(stable) => Delta::Nothing,
        None => Delta::Diverged,
    }
}

/// Text roles only: keystrokes on other elements (a web page, a Finder list)
/// fire single-key shortcuts. A selection range is no proof, since a focused
/// web page can report one with no input focused.
fn is_text_target(role: Option<&str>) -> bool {
    matches!(
        role,
        Some("AXTextField" | "AXTextArea" | "AXComboBox" | "AXSearchField")
    )
}

#[derive(Debug, PartialEq, Eq)]
enum FinishPlan {
    Fallback,
    TypeRest(String),
    /// Delete what was typed after its first `keep` bytes (the graphemes it
    /// shares with the final text), then type the rest of the final text.
    Replace {
        keep: usize,
    },
}

fn plan_finish(typed: &str, final_text: &str) -> FinishPlan {
    if typed.is_empty() {
        return FinishPlan::Fallback;
    }
    match final_text.strip_prefix(typed) {
        Some(rest) => FinishPlan::TypeRest(rest.to_string()),
        None => FinishPlan::Replace {
            keep: typed
                .graphemes(true)
                .zip(final_text.graphemes(true))
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| a.len())
                .sum(),
        },
    }
}

/// `before` (the text before the cursor) ends with `typed` and nothing is
/// selected. Compared in UTF-16 units, as AX reports them.
fn can_replace(before: &str, selected: Utf16Range, typed: &str) -> bool {
    let units = |s: &str| fold_typography(s).encode_utf16().collect::<Vec<u16>>();
    selected.length == 0 && !typed.is_empty() && units(before).ends_with(&units(typed))
}

/// Backspaces that delete exactly `typed`, or None when it holds a
/// multi-codepoint grapheme (ZWJ/skin-tone emoji, flag, combining mark): one
/// Backspace removes a whole grapheme, so a char count would eat user text.
fn backspace_count(typed: &str) -> Option<usize> {
    let chars = typed.chars().count();
    (typed.graphemes(true).count() == chars).then_some(chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_selection() -> Utf16Range {
        Utf16Range {
            location: 0,
            length: 0,
        }
    }

    #[test]
    fn stable_prefix_holds_back_last_words() {
        assert_eq!(stable_prefix("", 3), "");
        assert_eq!(stable_prefix("one two three", 3), "");
        assert_eq!(stable_prefix("one two three four", 3), "one");
        assert_eq!(stable_prefix("one two, three four five", 3), "one two,");
        assert_eq!(stable_prefix("one  two\tthree four five ", 3), "one  two");
        assert_eq!(stable_prefix("caffè è già pronto ok", 3), "caffè è");
    }

    #[test]
    fn next_delta_table() {
        assert_eq!(next_delta("", ""), Delta::Nothing);
        assert_eq!(next_delta("", "hello"), Delta::Append("hello".into()));
        assert_eq!(
            next_delta("hello", "hello world"),
            Delta::Append(" world".into())
        );
        assert_eq!(next_delta("hello world", "hello world"), Delta::Nothing);
        assert_eq!(next_delta("hello world", "hello"), Delta::Nothing);
        assert_eq!(next_delta("send it Thursday", "send it"), Delta::Nothing);
        assert_eq!(
            next_delta("send it Thursday", "send it Friday"),
            Delta::Diverged
        );
        assert_eq!(next_delta("hello", "Hello world"), Delta::Diverged);
    }

    #[test]
    fn is_text_target_table() {
        for role in ["AXTextField", "AXTextArea", "AXComboBox", "AXSearchField"] {
            assert!(is_text_target(Some(role)), "{role}");
        }
        // Web pages, lists and unknown roles are rejected even when they
        // report a selection range: the role alone decides.
        for role in [Some("AXWebArea"), Some("AXList"), Some("AXGroup"), None] {
            assert!(!is_text_target(role), "{role:?}");
        }
    }

    #[test]
    fn plan_finish_table() {
        assert_eq!(plan_finish("", "anything"), FinishPlan::Fallback);
        assert_eq!(plan_finish("", ""), FinishPlan::Fallback);
        assert_eq!(
            plan_finish("hello", "hello"),
            FinishPlan::TypeRest("".into())
        );
        assert_eq!(
            plan_finish("hello", "hello "),
            FinishPlan::TypeRest(" ".into())
        );
        assert_eq!(
            plan_finish("hello", "hello world."),
            FinishPlan::TypeRest(" world.".into())
        );
        assert_eq!(
            plan_finish("send it Thursday", "send it Friday"),
            FinishPlan::Replace { keep: 8 }
        );
        assert_eq!(
            plan_finish("hello world", "hello"),
            FinishPlan::Replace { keep: 5 }
        );
        assert_eq!(plan_finish("buy milk", ""), FinishPlan::Replace { keep: 0 });
        assert_eq!(
            plan_finish("The fairy to", "The ferry to Kilashanda Harbour "),
            FinishPlan::Replace { keep: 5 }
        );
        // Never split a grapheme: "e" + combining accent differs from plain "e".
        assert_eq!(
            plan_finish("cafe x", "cafe\u{0301} y"),
            FinishPlan::Replace { keep: 3 }
        );
    }

    #[test]
    fn can_replace_requires_typed_right_before_cursor() {
        assert!(can_replace("Dear Bob, send it", no_selection(), "send it"));
        assert!(can_replace("send it", no_selection(), "send it"));
        assert!(!can_replace("send it now", no_selection(), "send it"));
        assert!(!can_replace("it", no_selection(), "send it"));
        assert!(!can_replace("anything", no_selection(), ""));
        let selection = Utf16Range {
            location: 0,
            length: 2,
        };
        assert!(!can_replace("send it", selection, "send it"));
    }

    #[test]
    fn mismatch_summary_has_no_field_text() {
        assert_eq!(
            mismatch_summary("send\nit", no_selection(), "send it"),
            "read 7 of 7 UTF-16 units, 2 match back from the cursor, then field=newline typed=space, selection length 0"
        );
        assert_eq!(
            mismatch_summary("it", no_selection(), "send it"),
            "read 2 of 7 UTF-16 units, 2 match back from the cursor, then field=none typed=space, selection length 0"
        );
    }

    #[test]
    fn backspace_count_refuses_multi_codepoint_graphemes() {
        assert_eq!(backspace_count("send it"), Some(7));
        assert_eq!(backspace_count("caffè 😀"), Some(7));
        assert_eq!(backspace_count(""), Some(0));
        assert_eq!(backspace_count("ok 👍🏽"), None); // skin tone
        assert_eq!(backspace_count("👨\u{200D}👩\u{200D}👧"), None); // ZWJ
        assert_eq!(backspace_count("🇮🇹"), None); // flag
        assert_eq!(backspace_count("cafe\u{0301}"), None); // combining accent
    }

    #[test]
    fn committed_text_only_reaches_the_session_its_stream_started_in() {
        let (tx, rx) = mpsc::channel();
        let typer = LiveTyper {
            tx,
            current: Arc::new(AtomicU64::new(0)),
            last: Arc::new(AtomicU64::new(0)),
        };
        let committed = |generation| {
            typer.on_committed(generation, "hi", &OutputLanguageEvidence::Unknown, &[]);
            rx.try_iter()
                .filter(|c| matches!(c, Cmd::Committed { .. }))
                .count()
        };
        typer.begin(false);
        assert_eq!(typer.generation(), 0);
        assert_eq!(committed(0), 0);

        typer.begin(true);
        let first = typer.generation();
        assert_ne!(first, 0);
        assert_eq!(committed(first), 1);

        typer.cancel();
        typer.begin(true);
        let second = typer.generation();
        assert_ne!(second, first);
        assert_eq!(committed(first), 0, "cancelled stream's late result");
        assert_eq!(committed(second), 1);
    }

    #[test]
    fn can_replace_compares_utf16_and_folds_smart_quotes() {
        assert!(can_replace("x😀 ciao è", no_selection(), "😀 ciao è"));
        assert!(!can_replace("x😀 ciao è", no_selection(), "😃 ciao è"));
        assert!(can_replace(
            "I don\u{2019}t know",
            no_selection(),
            "don't know"
        ));
        assert!(can_replace(
            "say \u{201C}hi\u{201D}",
            no_selection(),
            "say \"hi\""
        ));
        assert_eq!(
            fold_typography("don\u{2019}t").encode_utf16().count(),
            "don't".encode_utf16().count()
        );
    }
}

//! Live typing: while a streaming dictation runs, types its stable prefix into
//! the focused field, then fixes the field up at the end with a verified
//! replace. Text is only ever deleted after Accessibility confirms it is still
//! the text we typed, right before the cursor, in the same field.
//!
//! Without an Accessibility target (non-macOS, unreadable field) no session
//! starts and the caller does the normal paste.

use crate::accessibility::{self, FocusedField, Utf16Range};
use crate::audio_toolkit::OutputLanguageEvidence;
use crate::clipboard::{send_return_key, with_enigo, write_text_to_clipboard};
use crate::input;
use crate::managers::transcription::post_process_transcription_text;
use crate::settings::{get_settings, AppSettings, ClipboardHandling};
use enigo::{Direction, Key, Keyboard};
use log::{debug, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// Trailing words never typed live: custom-word n-grams reach 3 words, so a
/// later word can still rewrite them.
const HOLDBACK_WORDS: usize = 3;
/// The target app may not have applied the last keystrokes when we read it.
const VERIFY_ATTEMPTS: usize = 5;
const VERIFY_RETRY: Duration = Duration::from_millis(60);

enum Cmd {
    Begin {
        live: bool,
    },
    Committed {
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
    /// Lets the engine thread skip cloning progress text when nobody types it.
    live: Arc<AtomicBool>,
}

impl LiveTyper {
    pub fn new(app: AppHandle) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || run(app, rx));
        Self {
            tx,
            live: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Called at every dictation start; replaces any session an earlier
    /// dictation left behind. Never blocks: Accessibility work runs on the
    /// typer thread.
    pub fn begin(&self, live: bool) {
        self.live.store(live, Ordering::Release);
        let _ = self.tx.send(Cmd::Begin { live });
    }

    /// Forwards the stream's raw committed text. Never blocks the engine thread.
    pub fn on_committed(
        &self,
        committed: &str,
        language: &OutputLanguageEvidence,
        languages: &[String],
    ) {
        if self.live.load(Ordering::Acquire) {
            let _ = self.tx.send(Cmd::Committed {
                text: committed.to_string(),
                language: language.clone(),
                languages: languages.to_vec(),
            });
        }
    }

    /// Delivers the final text. Blocks until the typer is done, so it must not
    /// run on the main thread (the typer types through it).
    pub fn finish(&self, final_text: String) -> Finish {
        self.live.store(false, Ordering::Release);
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::Finish(final_text, reply)).is_err() {
            return Finish::Fallback;
        }
        rx.recv().unwrap_or(Finish::Fallback)
    }

    /// Removes the typed text after verifying it. Never blocks.
    pub fn cancel(&self) {
        self.live.store(false, Ordering::Release);
        let _ = self.tx.send(Cmd::Cancel);
    }
}

struct Session {
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
            Cmd::Begin { live } => session = begin_session(&app, live),
            Cmd::Committed {
                text,
                language,
                languages,
            } => {
                if let Some(s) = session.as_mut().filter(|s| s.active) {
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

fn begin_session(app: &AppHandle, live: bool) -> Option<Session> {
    let settings = get_settings(app);
    if live || settings.auto_learn_enabled {
        accessibility::enable_manual_accessibility_for_focused_app();
    }
    if !live {
        return None;
    }
    // Chrome/Electron build their AX tree asynchronously after the flag.
    thread::sleep(Duration::from_millis(200));
    let Some(target) = accessibility::focused_field().filter(|f| !f.is_secure()) else {
        debug!("Live typing: no readable, non-secure focused field; using normal paste");
        return None;
    };
    Some(Session {
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
    let cleaned = post_process_transcription_text(raw, &s.settings, false, language, languages);
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
        FinishPlan::TypeRest(rest) => (0, rest),
        FinishPlan::Replace => (s.typed.chars().count(), text.clone()),
    };
    if !s.poisoned
        && verify_field(&s.target, &s.typed)
        && type_on_main(app, backspaces, &insert).is_ok()
    {
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
            with_enigo(app, |enigo| send_return_key(enigo, key))
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
    if !s.poisoned && verify_field(&s.target, &s.typed) && type_on_main(app, count, "").is_ok() {
        debug!("Live typing cancelled: removed {count} typed chars");
    } else {
        warn!("Live typing cancelled: field not verified, left {count} typed chars in place");
    }
}

fn on_target(target: &FocusedField) -> bool {
    accessibility::focused_field().is_some_and(|f| f.same_as(target))
}

/// The target still has focus and holds `typed` right before the cursor,
/// with nothing selected.
fn verify_field(target: &FocusedField, typed: &str) -> bool {
    let len = typed.encode_utf16().count();
    for attempt in 0..VERIFY_ATTEMPTS {
        if !on_target(target) {
            return false;
        }
        if let (Some(before), Some(selected)) =
            (target.text_before_cursor(len), target.selected_range())
        {
            if can_replace(&before, selected, typed) {
                return true;
            }
        }
        if attempt + 1 < VERIFY_ATTEMPTS {
            thread::sleep(VERIFY_RETRY);
        }
    }
    false
}

/// Sends `backspaces` Backspace clicks, then types `text`, on the main thread
/// (where enigo must run), waiting for it so `typed` stays exact.
fn type_on_main(app: &AppHandle, backspaces: usize, text: &str) -> Result<(), String> {
    if backspaces == 0 && text.is_empty() {
        return Ok(());
    }
    let text = text.to_string();
    on_main(app, move |app| {
        with_enigo(app, |enigo| {
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

#[derive(Debug, PartialEq, Eq)]
enum FinishPlan {
    Fallback,
    TypeRest(String),
    Replace,
}

fn plan_finish(typed: &str, final_text: &str) -> FinishPlan {
    if typed.is_empty() {
        return FinishPlan::Fallback;
    }
    match final_text.strip_prefix(typed) {
        Some(rest) => FinishPlan::TypeRest(rest.to_string()),
        None => FinishPlan::Replace,
    }
}

/// `before` (the text before the cursor) ends with `typed` and nothing is
/// selected. Compared in UTF-16 units, as AX reports them.
fn can_replace(before: &str, selected: Utf16Range, typed: &str) -> bool {
    let units = |s: &str| fold_typography(s).encode_utf16().collect::<Vec<u16>>();
    selected.length == 0 && !typed.is_empty() && units(before).ends_with(&units(typed))
}

/// Folds smart quotes and NBSP that apps substitute while we type. Each swap is
/// 1:1 in chars and UTF-16 units, so backspace counts stay exact.
// shortcut: duplicates correction_learner::fold_typography (private, under review); dedupe once that settles.
fn fold_typography(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            '\u{00A0}' => ' ',
            c => c,
        })
        .collect()
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
            FinishPlan::Replace
        );
        assert_eq!(plan_finish("hello world", "hello"), FinishPlan::Replace);
        assert_eq!(plan_finish("buy milk", ""), FinishPlan::Replace);
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

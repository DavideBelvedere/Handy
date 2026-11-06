//! Live typing: while a streaming dictation runs, types its stable prefix into
//! the focused field, then fixes the field up at the end with a verified
//! replace of the part that differs from the final text. Text is only ever deleted after Accessibility confirms it is still
//! the text we typed, right before the cursor, in the same field.
//!
//! The user may edit the field or move the cursor while dictating. Before
//! each delta the field is checked; on such a change the current [`Segment`]
//! is sealed (never touched again) and the next words start a new one at the
//! cursor. Only the last segment, if unedited, gets the final replace.
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
use tauri::AppHandle;
use unicode_segmentation::UnicodeSegmentation;

/// Trailing words never typed live: custom-word n-grams reach 3 words, so a
/// later word can still rewrite them.
const HOLDBACK_WORDS: usize = 3;
/// The target app may not have applied the last keystrokes when we read it.
const VERIFY_ATTEMPTS: usize = 5;
const VERIFY_RETRY: Duration = Duration::from_millis(60);
/// How long a Chrome/Electron app may take to build its AX tree after
/// AXManualAccessibility; a native field is found on the first read.
pub(crate) const FIELD_WAIT: Duration = Duration::from_millis(2_500);

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
    /// The live segment's final text and its text before Post Process.
    Finish {
        text: String,
        raw: String,
        reply: mpsc::Sender<Finish>,
    },
    SegmentStart(mpsc::Sender<usize>),
    Cancel,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Finish {
    /// Live typing typed the output; holds what the last step inserted, for
    /// the correction watcher.
    Handled(String),
    /// The field could not be verified, so the output went to the clipboard.
    Copied,
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

    /// Words of the final transcription that belong to sealed segments (0
    /// when nothing was sealed): only the rest, [`split_at_word`], is Post
    /// Processed and passed to [`Self::finish`]. Blocks until queued commits
    /// are typed; never call it on the main thread.
    pub fn segment_start(&self) -> usize {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::SegmentStart(reply)).is_err() {
            return 0;
        }
        rx.recv().unwrap_or(0)
    }

    /// Delivers the live segment's final text, and `raw`, the same text before
    /// Post Process, typed instead when the user edited the segment. Blocks
    /// until the typer is done, so it must not run on the main thread (the
    /// typer types through it).
    pub fn finish(&self, text: String, raw: String) -> Finish {
        self.current.store(0, Ordering::Release);
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::Finish { text, raw, reply }).is_err() {
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
    segment: Segment,
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
            Cmd::Finish { text, raw, reply } => {
                let outcome = match session.take() {
                    Some(s) => finish_session(&app, s, &text, &raw),
                    None => Finish::Fallback,
                };
                let _ = reply.send(outcome);
            }
            Cmd::SegmentStart(reply) => {
                let _ = reply.send(session.as_ref().map_or(0, |s| s.segment.start_words));
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
    let (field, waited) = accessibility::wait_for_focused_field(FIELD_WAIT, field_settled);
    let target = match field {
        None => {
            debug!("Live typing: no focused element after {waited:?}; using normal paste");
            return None;
        }
        Some(f) if f.is_secure() => {
            debug!("Live typing: focused field is secure; using normal paste");
            return None;
        }
        Some(f) if !is_text_target(f.role().as_deref()) => {
            debug!(
                "Live typing: focused role {:?} (subrole {:?}) is not a text field after {waited:?}; using normal paste",
                f.role(),
                f.subrole()
            );
            return None;
        }
        Some(f) => f,
    };
    debug!("Live typing: text field found after {waited:?}");
    Some(Session {
        generation,
        target,
        segment: Segment::default(),
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
    let delta = segment_delta(s, &cleaned);
    s.segment.seen_words = word_count(&cleaned);
    let Some(delta) = delta else { return };
    match type_on_main(app, 0, &delta) {
        Ok(()) => s.segment.typed.push_str(&delta),
        Err(e) => {
            warn!("Live typing: typing failed ({e}); stopping");
            s.active = false;
            s.poisoned = true;
        }
    }
}

/// What to type for `cleaned` at the cursor, after checking the field: a
/// user edit or cursor move seals the segment first.
fn segment_delta(s: &mut Session, cleaned: &str) -> Option<String> {
    let delta = match s.segment.delta(cleaned) {
        Delta::Nothing => return None,
        Delta::Diverged => {
            debug!(
                "Live typing: cleaned text diverged after {} typed chars; stopping",
                s.segment.typed.chars().count()
            );
            s.active = false;
            return None;
        }
        Delta::Append(delta) => delta,
    };
    match verify_field(&s.target, &s.segment.typed) {
        FieldCheck::Verified | FieldCheck::Unreadable => Some(delta),
        FieldCheck::Moved => {
            debug!("Live typing: focus moved; stopping");
            s.active = false;
            None
        }
        // Typing would replace the user's selection: wait for the next commit.
        FieldCheck::Selection => None,
        FieldCheck::Edited => {
            let (carry_tail, lead) = cursor_context(s.target.text_near_cursor());
            let dropped = s.segment.seal(carry_tail, lead);
            debug!(
                "Live typing: field edited; new segment at the cursor from word {} ({dropped} pending words dropped)",
                s.segment.start_words
            );
            match s.segment.delta(cleaned) {
                Delta::Append(delta) => Some(delta),
                _ => None,
            }
        }
    }
}

fn finish_session(app: &AppHandle, s: Session, final_text: &str, raw_text: &str) -> Finish {
    let seg = &s.segment;
    if seg.typed.is_empty() && seg.start_words == 0 {
        // Nothing was typed live: the normal paste does the job.
        return Finish::Fallback;
    }
    let mut text = seg.with_lead(final_text);
    if s.settings.append_trailing_space && !text.is_empty() {
        text.push(' ');
    }
    let check = if s.poisoned {
        None
    } else if seg.typed == text {
        // The field already holds the whole output: nothing to read.
        Some(FieldCheck::Verified)
    } else {
        Some(verify_field(&s.target, &seg.typed))
    };
    let mut edited_rest = String::new();
    if check == Some(FieldCheck::Edited) {
        // Kept raw: Post Process must not undo the user's edits.
        let (carry_tail, lead) = cursor_context(s.target.text_near_cursor());
        if carry_tail {
            edited_rest = with_lead(lead, skip_words(raw_text, word_count(&seg.typed)));
            if s.settings.append_trailing_space && !edited_rest.is_empty() {
                edited_rest.push(' ');
            }
        }
        debug!(
            "Live typing: segment edited before finish; {} raw chars typed at the cursor",
            edited_rest.chars().count()
        );
    }
    let step = check.map_or(FinishStep::Copy, |check| {
        plan_segment_finish(check, &seg.typed, &text, edited_rest)
    });
    if let FinishStep::Type { backspaces, insert } = &step {
        if type_on_main(app, *backspaces, insert).is_ok() {
            debug!(
                "Live typing finished: {} typed, {} backspaces, {} chars inserted",
                seg.typed.chars().count(),
                backspaces,
                insert.chars().count()
            );
            // An edited segment only got `insert`; otherwise it now holds `text`.
            let inserted = if check == Some(FieldCheck::Edited) {
                insert.trim().to_string()
            } else {
                text
            };
            finish_like_paste(app, &s.settings, inserted.clone());
            return Finish::Handled(inserted);
        }
    }
    if final_text.is_empty() {
        return Finish::Handled(String::new());
    }
    warn!(
        "Live typing: field not verified after {} typed chars; final text copied to clipboard",
        seg.typed.chars().count()
    );
    let final_text = final_text.to_string();
    if let Err(e) = on_main(app, move |app| write_text_to_clipboard(app, &final_text)) {
        warn!("Live typing: clipboard fallback failed: {e}");
    }
    Finish::Copied
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

/// Removes the live segment only: sealed ones hold the user's edits.
fn cancel_session(app: &AppHandle, s: Session) {
    let typed = &s.segment.typed;
    if typed.is_empty() {
        return;
    }
    let count = typed.chars().count();
    if !s.poisoned
        && backspace_count(typed).is_some()
        && verify_field(&s.target, typed) == FieldCheck::Verified
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldCheck {
    /// The target has focus and holds `typed` right before the cursor, with
    /// nothing selected.
    Verified,
    /// Focus left the target field.
    Moved,
    /// The user selected text in the target: typing would replace it.
    Selection,
    /// The target was read and never held `typed` right before the cursor:
    /// the user edited it or moved the cursor.
    Edited,
    /// The target has focus but its text could not be read.
    Unreadable,
}

fn verify_field(target: &FocusedField, typed: &str) -> FieldCheck {
    let len = typed.encode_utf16().count();
    let mut last_read = None;
    for attempt in 0..VERIFY_ATTEMPTS {
        if !on_target(target) {
            debug!("Live typing: verify failed, focus moved");
            return FieldCheck::Moved;
        }
        if let (Some(before), Some(selected)) =
            (target.text_before_cursor(len), target.selected_range())
        {
            if selected.length > 0 {
                return FieldCheck::Selection;
            }
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
            FieldCheck::Edited
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

/// The focused element is final: anything but the web area or group a
/// Chrome/Electron app reports while its AX tree is still building. A
/// secure field ends the wait too: it never becomes a target.
pub(crate) fn field_settled(field: &FocusedField) -> bool {
    field.is_secure() || !is_building_role(field.role().as_deref())
}

fn is_building_role(role: Option<&str>) -> bool {
    matches!(role, None | Some("AXWebArea" | "AXGroup"))
}

#[derive(Debug, PartialEq, Eq)]
enum FinishPlan {
    TypeRest(String),
    /// Delete what was typed after its first `keep` bytes (the graphemes it
    /// shares with the final text), then type the rest of the final text.
    Replace {
        keep: usize,
    },
}

fn plan_finish(typed: &str, final_text: &str) -> FinishPlan {
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
/// selected. Compared in UTF-16 units, as AX reports them. An empty `typed`
/// (a segment that typed nothing yet) only needs no selection.
fn can_replace(before: &str, selected: Utf16Range, typed: &str) -> bool {
    let units = |s: &str| fold_typography(s).encode_utf16().collect::<Vec<u16>>();
    selected.length == 0 && units(before).ends_with(&units(typed))
}

/// The text the session types at the cursor. A user edit or cursor move
/// seals it: it is never revised again, and the next words start a new one.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Segment {
    /// Words of the cleaned dictation before this segment: typed into sealed
    /// segments, or dropped as a sealed segment's pending tail.
    start_words: usize,
    /// "" or " ": separates the segment from the user's text before the cursor.
    lead: &'static str,
    /// What this segment typed, `lead` included.
    typed: String,
    /// Words in the cleaned text at the last commit.
    seen_words: usize,
}

impl Segment {
    /// Delta to type for `cleaned`, whose last [`HOLDBACK_WORDS`] words are a
    /// tentative tail that is never typed, so revising it deletes nothing.
    fn delta(&self, cleaned: &str) -> Delta {
        let stable = stable_prefix(cleaned, HOLDBACK_WORDS);
        next_delta(
            &self.typed,
            &self.with_lead(skip_words(stable, self.start_words)),
        )
    }

    fn with_lead(&self, text: &str) -> String {
        with_lead(self.lead, text)
    }

    /// Seals this segment and starts the next at the cursor, `lead` apart from
    /// the text before it. Its pending tail (words seen but not typed) goes to
    /// the new segment when the cursor is at the end of the text, where the
    /// user kept writing; otherwise it is dropped. Returns the dropped words.
    fn seal(&mut self, carry_tail: bool, lead: &'static str) -> usize {
        let typed_end = self.start_words + word_count(&self.typed);
        let next = if carry_tail {
            typed_end
        } else {
            self.seen_words.max(typed_end)
        };
        *self = Segment {
            start_words: next,
            lead,
            typed: String::new(),
            seen_words: self.seen_words,
        };
        next - typed_end
    }
}

fn with_lead(lead: &str, text: &str) -> String {
    let text = text.trim_start();
    if text.is_empty() {
        String::new()
    } else {
        format!("{lead}{text}")
    }
}

fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

/// Byte offset of word `n` (0-based, whitespace-separated) in `text`, or its
/// length when it has fewer words.
fn word_offset(text: &str, n: usize) -> usize {
    let mut words = 0;
    let mut prev_ws = true;
    for (i, c) in text.char_indices() {
        if !c.is_whitespace() && prev_ws {
            if words == n {
                return i;
            }
            words += 1;
        }
        prev_ws = c.is_whitespace();
    }
    text.len()
}

/// `text` from its word `n` on.
fn skip_words(text: &str, n: usize) -> &str {
    &text[word_offset(text, n)..]
}

/// Splits the final transcription into the sealed segments' words (first
/// [`LiveTyper::segment_start`] words) and the live segment's.
pub fn split_at_word(text: &str, n: usize) -> (&str, &str) {
    let (sealed, live) = text.split_at(if n == 0 { 0 } else { word_offset(text, n) });
    (sealed.trim_end(), live)
}

/// From the text around the cursor and the cursor's UTF-16 offset in it:
/// whether the cursor is at the end of the text (only whitespace after it),
/// and the lead a new segment needs ("" after whitespace or at the start).
/// Unreadable: drop the tail, no lead.
fn cursor_context(near: Option<(String, usize)>) -> (bool, &'static str) {
    let Some((text, cursor)) = near else {
        return (false, "");
    };
    let units: Vec<u16> = text.encode_utf16().collect();
    let cursor = cursor.min(units.len());
    let before = String::from_utf16_lossy(&units[..cursor]);
    let after = String::from_utf16_lossy(&units[cursor..]);
    let lead = match before.chars().next_back() {
        Some(c) if !c.is_whitespace() => " ",
        _ => "",
    };
    (after.trim().is_empty(), lead)
}

#[derive(Debug, PartialEq, Eq)]
enum FinishStep {
    /// Send `backspaces` Backspaces, then type `insert`.
    Type { backspaces: usize, insert: String },
    /// Leave the field alone; the final text goes to the clipboard.
    Copy,
}

/// How to end the live segment, which typed `typed`, given the field check.
/// Only a verified, unedited segment is replaced with `text` (the final,
/// Post Processed text). An edited one keeps the user's edits and only gets
/// `edited_rest`, raw text typed at the cursor.
fn plan_segment_finish(
    check: FieldCheck,
    typed: &str,
    text: &str,
    edited_rest: String,
) -> FinishStep {
    let plan = plan_finish(typed, text);
    match (check, plan) {
        (FieldCheck::Verified | FieldCheck::Unreadable, FinishPlan::TypeRest(rest)) => {
            FinishStep::Type {
                backspaces: 0,
                insert: rest,
            }
        }
        // Deleting needs a verified read and an exact Backspace count.
        (FieldCheck::Verified, FinishPlan::Replace { keep }) => {
            match backspace_count(&typed[keep..]) {
                Some(backspaces) => FinishStep::Type {
                    backspaces,
                    insert: text[keep..].to_string(),
                },
                None => FinishStep::Copy,
            }
        }
        (FieldCheck::Edited, _) => FinishStep::Type {
            backspaces: 0,
            insert: edited_rest,
        },
        _ => FinishStep::Copy,
    }
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
    fn only_a_building_chromium_role_keeps_waiting() {
        for role in [None, Some("AXWebArea"), Some("AXGroup")] {
            assert!(is_building_role(role), "{role:?}");
        }
        for role in [Some("AXTextArea"), Some("AXList"), Some("AXTable")] {
            assert!(!is_building_role(role), "{role:?}");
        }
    }

    #[test]
    fn plan_finish_table() {
        assert_eq!(
            plan_finish("", "anything"),
            FinishPlan::TypeRest("anything".into())
        );
        assert_eq!(plan_finish("", ""), FinishPlan::TypeRest("".into()));
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
        // A segment that typed nothing yet only needs no selection.
        assert!(can_replace("anything", no_selection(), ""));
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

    /// A segment fed the cleaned commits in `commits`, typing every delta.
    fn typed_through(seg: &mut Segment, commits: &[&str]) {
        for cleaned in commits {
            if let Delta::Append(delta) = seg.delta(cleaned) {
                seg.typed.push_str(&delta);
            }
            seg.seen_words = word_count(cleaned);
        }
    }

    #[test]
    fn unedited_segment_keeps_appending() {
        let mut seg = Segment::default();
        typed_through(
            &mut seg,
            &[
                "send it",
                "send it to Bob on",
                "send it to Bob on Friday at noon",
            ],
        );
        assert_eq!(seg.typed, "send it to Bob on");
        assert_eq!(seg.start_words, 0);
        assert_eq!(
            seg.delta("send it to Bob on Friday at noon"),
            Delta::Nothing
        );
    }

    #[test]
    fn asr_revising_the_tentative_tail_deletes_nothing() {
        let mut seg = Segment::default();
        typed_through(&mut seg, &["send it to Bob on Thursday"]);
        assert_eq!(seg.typed, "send it to");
        // The held-back "Bob on Thursday" is revised: never typed, so no
        // deletion and no divergence.
        assert_eq!(
            seg.delta("send it to Rob on Friday at noon"),
            Delta::Append(" Rob on".into())
        );
    }

    #[test]
    fn seal_after_edit_mid_text_drops_the_pending_tail() {
        let mut seg = Segment::default();
        typed_through(&mut seg, &["Dear Bob send it Thursday at noon"]);
        assert_eq!(seg.typed, "Dear Bob send it");
        // The user fixed "Bob" and left the cursor after it, mid-text: the
        // pending "Thursday at noon" would land in the wrong place.
        let (carry, lead) = cursor_context(Some(("Dear Rob send it".into(), 8)));
        assert_eq!((carry, lead), (false, " "));
        assert_eq!(seg.seal(carry, lead), 3);
        assert_eq!(seg.start_words, 7);
        assert!(seg.typed.is_empty(), "sealed text is never deleted");
        // Only words spoken after the edit reach the new segment.
        assert_eq!(
            seg.delta("Dear Bob send it Thursday at noon and also call Anna back please"),
            Delta::Append(" and also call".into())
        );
    }

    #[test]
    fn seal_with_cursor_at_end_carries_the_pending_tail() {
        let mut seg = Segment::default();
        typed_through(&mut seg, &["send it Thursday at noon please"]);
        assert_eq!(seg.typed, "send it Thursday");
        // The user retyped the last word at the end of the field and kept going.
        let (carry, lead) = cursor_context(Some(("send it Friday\n".into(), 14)));
        assert_eq!((carry, lead), (true, " "));
        assert_eq!(seg.seal(carry, lead), 0);
        assert_eq!(
            seg.delta("send it Thursday at noon please and more words here"),
            Delta::Append(" at noon please and".into())
        );
    }

    #[test]
    fn edit_inside_sealed_text_never_reaches_the_new_segment() {
        let mut seg = Segment::default();
        typed_through(&mut seg, &["one two three four five"]);
        seg.seal(false, "");
        let sealed_at = seg.start_words;
        // Whatever the user did to the sealed words, the new segment only
        // ever sees words after them, and typed nothing deletable.
        typed_through(&mut seg, &["one two three four five six seven eight nine"]);
        assert_eq!(seg.start_words, sealed_at);
        assert_eq!(seg.typed, "six");
        assert_eq!(
            plan_segment_finish(FieldCheck::Edited, &seg.typed, "Six seven.", String::new()),
            FinishStep::Type {
                backspaces: 0,
                insert: String::new()
            }
        );
    }

    #[test]
    fn cursor_context_reads_lead_and_end() {
        assert_eq!(cursor_context(None), (false, ""));
        assert_eq!(cursor_context(Some(("".into(), 0))), (true, ""));
        assert_eq!(cursor_context(Some(("hi ".into(), 3))), (true, ""));
        assert_eq!(cursor_context(Some(("hi there".into(), 2))), (false, " "));
        // UTF-16 offsets: the emoji is two units.
        assert_eq!(cursor_context(Some(("😀x".into(), 2))), (false, " "));
        assert_eq!(cursor_context(Some(("x".into(), 99))), (true, " "));
    }

    #[test]
    fn split_at_word_cuts_the_final_text_at_the_segment() {
        assert_eq!(split_at_word("send it now", 0), ("", "send it now"));
        assert_eq!(split_at_word("send it  now", 2), ("send it", "now"));
        assert_eq!(split_at_word("send it", 5), ("send it", ""));
        assert_eq!(skip_words("a b c", 1), "b c");
        assert_eq!(word_offset("  a b", 0), 2);
    }

    #[test]
    fn post_process_replaces_only_an_unedited_segment() {
        // Unedited: the LLM text replaces what was typed live.
        assert_eq!(
            plan_segment_finish(
                FieldCheck::Verified,
                "send it to bob",
                "Send it to Bob.",
                String::new()
            ),
            FinishStep::Type {
                backspaces: 14,
                insert: "Send it to Bob.".into()
            }
        );
        // Edited: the user's text stays; only the raw rest goes in at the cursor.
        assert_eq!(
            plan_segment_finish(
                FieldCheck::Edited,
                "send it to bob",
                "Send it to Bob.",
                " on Friday".into()
            ),
            FinishStep::Type {
                backspaces: 0,
                insert: " on Friday".into()
            }
        );
        // Unreadable: append only, never delete.
        assert_eq!(
            plan_segment_finish(
                FieldCheck::Unreadable,
                "send it",
                "send it now",
                String::new()
            ),
            FinishStep::Type {
                backspaces: 0,
                insert: " now".into()
            }
        );
        assert_eq!(
            plan_segment_finish(FieldCheck::Unreadable, "send it", "Send it.", String::new()),
            FinishStep::Copy
        );
        for check in [FieldCheck::Moved, FieldCheck::Selection] {
            assert_eq!(
                plan_segment_finish(check, "send it", "Send it.", String::new()),
                FinishStep::Copy
            );
        }
        // A new segment that typed nothing yet gets its whole final text.
        assert_eq!(
            plan_segment_finish(FieldCheck::Verified, "", " Call Anna.", String::new()),
            FinishStep::Type {
                backspaces: 0,
                insert: " Call Anna.".into()
            }
        );
        // Never split a multi-codepoint grapheme with Backspace.
        assert_eq!(
            plan_segment_finish(FieldCheck::Verified, "ok 👍🏽", "Ok.", String::new()),
            FinishStep::Copy
        );
    }
}

//! Live typing: while a streaming dictation runs, types its stable prefix into
//! the focused field, then fixes the field up at the end with a verified
//! replace of the part that differs from the final text. Text is only ever deleted after Accessibility confirms it is still
//! the text we typed, right before the cursor, in the same field.
//!
//! The user may edit the field or move the cursor while dictating. Before
//! each delta the field is checked; on such a change the current [`Segment`]
//! is sealed (never touched again) and the next words start a new one at the
//! cursor. Only the last segment, if unedited, gets the final replace. A
//! selection seals the segment too, and the next words replace it.
//!
//! When the speaker pauses, the held-back words and the model's tentative ones
//! are typed too, as the segment's tail; until they become stable they may be
//! revised, with the same verified delete.
//!
//! Without an Accessibility target (Linux, unreadable field) no session
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
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
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
/// Silence after which the held-back and tentative words are typed.
const PAUSE_FLUSH: Duration = Duration::from_millis(400);

enum Cmd {
    /// `generation` 0 = not live.
    Begin {
        generation: u64,
    },
    Committed {
        generation: u64,
        text: String,
        tentative: String,
        language: OutputLanguageEvidence,
        languages: Vec<String>,
    },
    /// The live segment's final text and its text before Post Process.
    Finish {
        text: String,
        raw: String,
        copy_only: Option<String>,
        reply: mpsc::Sender<Finish>,
    },
    SegmentStart(mpsc::Sender<usize>),
    Cancel,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Finish {
    /// Live typing typed the output. Holds, for the correction watcher, what
    /// the last step inserted and each segment the user edited while
    /// dictating.
    Handled(String, Vec<EditedSegment>),
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

    /// Forwards the stream's raw committed and tentative text. Never blocks
    /// the engine thread.
    pub fn on_committed(
        &self,
        generation: u64,
        committed: &str,
        tentative: &str,
        language: &OutputLanguageEvidence,
        languages: &[String],
    ) {
        if generation != 0 && generation == self.generation() {
            let _ = self.tx.send(Cmd::Committed {
                generation,
                text: committed.to_string(),
                tentative: tentative.to_string(),
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
    /// Post Process, typed instead when the user edited the segment. With
    /// `copy_only`, typed text is never rewritten (only a missing tail is
    /// appended to an untouched field) and that text goes to the clipboard. Blocks until the typer is done, so it must not run
    /// on the main thread (the typer types through it).
    pub fn finish(&self, text: String, raw: String, copy_only: Option<String>) -> Finish {
        self.current.store(0, Ordering::Release);
        let (reply, rx) = mpsc::channel();
        let cmd = Cmd::Finish {
            text,
            raw,
            copy_only,
            reply,
        };
        if self.tx.send(cmd).is_err() {
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
    /// The latest raw text with the model's tentative words.
    latest: Option<Pending>,
    /// When `latest` last changed, and whether a pause has typed it since.
    changed_at: Instant,
    flushed: bool,
    /// The field around the cursor as of the last verified read, kept up to
    /// date with what was typed since: the "before" of a user edit.
    around: Option<Around>,
    /// Sealed segments the user edited, for auto-learn.
    sealed: Vec<EditedSegment>,
}

/// A segment the user edited while dictating: (typed text, the field before
/// the edit, the field when the edit was found, before any more typing).
pub type EditedSegment = (String, String, String);

/// The field's text before and after the cursor.
type Around = (String, String);

#[derive(Clone)]
struct Pending {
    raw: String,
    language: OutputLanguageEvidence,
    languages: Vec<String>,
}

impl Session {
    fn flush_due(&self) -> Option<Instant> {
        (self.active && !self.flushed && self.latest.is_some())
            .then(|| self.changed_at + PAUSE_FLUSH)
    }
}

fn run(app: AppHandle, rx: mpsc::Receiver<Cmd>) {
    let mut session: Option<Session> = None;
    loop {
        let was_live = session.as_ref().is_some_and(|s| s.active);
        let due = session.as_ref().and_then(Session::flush_due);
        let cmd = match due {
            Some(due) => match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
                Ok(cmd) => Some(cmd),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => break,
            },
        };
        let Some(cmd) = cmd else {
            if let Some(s) = session.as_mut() {
                flush_tail(&app, s);
            }
            continue;
        };
        match cmd {
            Cmd::Begin { generation } => {
                session = begin_session(&app, generation);
                show_live_in_overlay(&app, session.is_some());
                continue;
            }
            Cmd::Committed {
                generation,
                text,
                tentative,
                language,
                languages,
            } => {
                // A cancelled stream's late results belong to an older session.
                if let Some(s) = session
                    .as_mut()
                    .filter(|s| s.active && s.generation == generation)
                {
                    let raw = if tentative.trim().is_empty() {
                        text.clone()
                    } else {
                        format!("{} {}", text.trim_end(), tentative.trim_start())
                    };
                    if s.latest.as_ref().map(|p| &p.raw) != Some(&raw) {
                        s.changed_at = Instant::now();
                        s.flushed = false;
                        s.latest = Some(Pending {
                            raw,
                            language: language.clone(),
                            languages: languages.clone(),
                        });
                    }
                    type_committed(&app, s, text, &language, &languages);
                }
            }
            Cmd::Finish {
                text,
                raw,
                copy_only,
                reply,
            } => {
                let outcome = match session.take() {
                    Some(s) => match copy_only {
                        Some(copy) => finish_copy_only(&app, s, &text, &copy),
                        None => finish_session(&app, s, &text, &raw),
                    },
                    None => Finish::Fallback,
                };
                let _ = reply.send(outcome);
            }
            Cmd::SegmentStart(reply) => {
                // A selection made after the last commit: the rest replaces it.
                if let Some(s) = session.as_mut().filter(|s| s.active) {
                    seal_on_selection(s);
                    if let Some(p) = s.latest.clone() {
                        let cleaned = post_process_transcription_text(
                            p.raw,
                            &s.settings,
                            &[],
                            &p.language,
                            &p.languages,
                        );
                        s.segment.match_case(&cleaned, &s.settings.custom_words);
                    }
                }
                let _ = reply.send(session.as_ref().map_or(0, |s| s.segment.start_words));
            }
            Cmd::Cancel => {
                if let Some(s) = session.take() {
                    cancel_session(&app, s);
                }
            }
        }
        if was_live && !session.as_ref().is_some_and(|s| s.active) {
            show_live_in_overlay(&app, false);
        }
    }
}

/// Tells the overlay whether the text is appearing in the field, where it
/// then shows no transcript of its own.
fn show_live_in_overlay(app: &AppHandle, live: bool) {
    let _ = app.emit_to("recording_overlay", "live-typing", live);
}

/// After a pause, types what is still held back (the stable prefix's last
/// words and the model's tentative ones) as the segment's revisable tail.
fn flush_tail(app: &AppHandle, s: &mut Session) {
    s.flushed = true;
    let Some(p) = s.latest.clone() else { return };
    let cleaned =
        post_process_transcription_text(p.raw, &s.settings, &[], &p.language, &p.languages);
    if let Some(edit) = segment_delta(s, &cleaned, true) {
        apply_edit(app, s, edit, true);
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
        latest: None,
        changed_at: Instant::now(),
        flushed: false,
        around: None,
        sealed: Vec::new(),
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
    let edit = segment_delta(s, &cleaned, false);
    s.segment.seen_words = word_count(&cleaned);
    if let Some(edit) = edit {
        apply_edit(app, s, edit, false);
    }
}

fn apply_edit(app: &AppHandle, s: &mut Session, edit: Delta, flush: bool) {
    let (backspaces, insert) = match &edit {
        Delta::Append(insert) => (0, insert.as_str()),
        Delta::Revise {
            backspaces, insert, ..
        } => (*backspaces, insert.as_str()),
        Delta::Nothing | Delta::Diverged => return,
    };
    match type_on_main(app, backspaces, insert) {
        Ok(()) => {
            if let Some((before, _)) = s.around.as_mut() {
                for _ in 0..backspaces {
                    before.pop();
                }
                before.push_str(insert);
            }
            s.segment.apply(edit, flush);
        }
        Err(e) => {
            warn!("Live typing: typing failed ({e}); stopping");
            s.active = false;
            s.poisoned = true;
        }
    }
}

/// The edit to make for `cleaned` at the cursor (its stable prefix, or all
/// of it when `flush`), after checking the field: a user edit or cursor move
/// seals the segment first.
fn segment_delta(s: &mut Session, cleaned: &str, flush: bool) -> Option<Delta> {
    // Checked even with nothing to type, so only words spoken after the
    // selection replace it.
    seal_on_selection(s);
    s.segment.match_case(cleaned, &s.settings.custom_words);
    let edit = match s.segment.plan(cleaned, flush) {
        Delta::Nothing => return None,
        Delta::Diverged => {
            debug!(
                "Live typing: cleaned text diverged after {} typed chars; stopping",
                s.segment.typed.chars().count()
            );
            s.active = false;
            return None;
        }
        edit => edit,
    };
    match verify_field(&s.target, &s.segment.typed) {
        FieldCheck::Verified => {
            s.around = s.target.text_near_cursor().map(split_at_cursor);
            Some(edit)
        }
        // Appending deletes nothing, so an unreadable field only needs focus;
        // but a flushed tail could never be revised there.
        FieldCheck::Unreadable if !flush && matches!(edit, Delta::Append(_)) => {
            s.around = None;
            Some(edit)
        }
        FieldCheck::Unreadable if flush => None,
        FieldCheck::Unreadable => {
            debug!("Live typing: can't revise the tail of an unreadable field; stopping");
            s.active = false;
            None
        }
        FieldCheck::Moved => {
            debug!("Live typing: focus moved; stopping");
            s.active = false;
            None
        }
        // A sealed segment's first words replace the user's selection; the
        // field read holds the selected text, so it can't track the typing.
        FieldCheck::Selection if s.segment.typed.is_empty() => {
            s.around = None;
            Some(edit)
        }
        // Selected since the check above: the next commit seals.
        FieldCheck::Selection => None,
        FieldCheck::Edited => {
            seal_segment(s, true);
            s.segment.match_case(cleaned, &s.settings.custom_words);
            match s.segment.plan(cleaned, flush) {
                edit @ Delta::Append(_) => Some(edit),
                _ => None,
            }
        }
    }
}

/// Seals the segment when the user selected text, so the words spoken next
/// replace the selection. The held-back words were spoken before it: dropped.
fn seal_on_selection(s: &mut Session) {
    if s.poisoned
        || s.segment.typed.is_empty()
        || !s.target.selected_range().is_some_and(|r| r.length > 0)
    {
        return;
    }
    if verify_field(&s.target, &s.segment.typed) == FieldCheck::Selection {
        seal_segment(s, false);
    }
}

/// Seals the segment after a user edit or selection; the next words start a
/// new one at the cursor. `may_carry`: the pending tail may move to it.
fn seal_segment(s: &mut Session, may_carry: bool) {
    let around = s.target.text_near_cursor().map(split_at_cursor);
    s.sealed.extend(edited_segment(
        s.around.take(),
        &s.segment.typed,
        around.as_ref(),
    ));
    let (carry_tail, lead) = cursor_context(around.as_ref());
    s.around = around;
    let dropped = s.segment.seal(may_carry && carry_tail, lead);
    s.segment.field_case = field_case(s.around.as_ref());
    debug!(
        "Live typing: field edited or selected; new segment at the cursor from word {} ({dropped} pending words dropped)",
        s.segment.start_words
    );
}

fn finish_session(app: &AppHandle, mut s: Session, final_text: &str, raw_text: &str) -> Finish {
    let mut sealed = std::mem::take(&mut s.sealed);
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
        let around = s.target.text_near_cursor().map(split_at_cursor);
        sealed.extend(edited_segment(s.around.take(), &seg.typed, around.as_ref()));
        let (carry_tail, lead) = cursor_context(around.as_ref());
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
            return Finish::Handled(inserted, sealed);
        }
    }
    if final_text.is_empty() {
        return Finish::Handled(String::new(), sealed);
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

/// Never rewrites what is in the field and copies `copy`, the whole
/// dictation, to the clipboard. The only change to the field: words still
/// held back are appended when the field is exactly as live typing left it.
fn finish_copy_only(app: &AppHandle, mut s: Session, final_text: &str, copy: &str) -> Finish {
    let mut sealed = std::mem::take(&mut s.sealed);
    let seg = &s.segment;
    if seg.typed.is_empty() && seg.start_words == 0 {
        return Finish::Fallback;
    }
    let mut text = seg.with_lead(final_text);
    if s.settings.append_trailing_space && !text.is_empty() {
        text.push(' ');
    }
    let check = (!s.poisoned).then(|| verify_field(&s.target, &seg.typed));
    let mut inserted = String::new();
    match check {
        Some(check @ (FieldCheck::Verified | FieldCheck::Selection)) => {
            inserted = seg.typed.clone();
            if let FinishStep::Type {
                backspaces: 0,
                insert,
            } = plan_segment_finish(check, &seg.typed, &text, String::new())
            {
                if !insert.is_empty() && type_on_main(app, 0, &insert).is_ok() {
                    inserted.push_str(&insert);
                }
            }
        }
        // Learned from like a sealed segment; the field stays as the user left it.
        Some(FieldCheck::Edited) => {
            let around = s.target.text_near_cursor().map(split_at_cursor);
            sealed.extend(edited_segment(s.around.take(), &seg.typed, around.as_ref()));
        }
        _ => {}
    }
    if !copy.is_empty() {
        let copy = copy.to_string();
        if let Err(e) = on_main(app, move |app| write_text_to_clipboard(app, &copy)) {
            warn!("Live typing: copy to clipboard failed: {e}");
        }
    }
    debug!(
        "Live typing finished without rewriting the field ({check:?}); {} chars copied",
        copy.chars().count()
    );
    Finish::Handled(inserted, sealed)
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

/// Removes what was typed, unless the user edited, selected or moved the
/// cursor in the field while dictating (a sealed segment): their changes and
/// the text around them stay.
fn cancel_session(app: &AppHandle, s: Session) {
    let typed = &s.segment.typed;
    if typed.is_empty() {
        return;
    }
    if s.segment.start_words > 0 {
        debug!("Live typing cancelled: the field was edited, left the typed text in place");
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
    /// The user selected text in the target: typing replaces it, so only a
    /// segment that typed nothing yet types over it.
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
    /// Delete the segment's tail after its first `keep` bytes
    /// (`backspaces` of it), then type `insert`.
    Revise {
        keep: usize,
        backspaces: usize,
        insert: String,
    },
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
    /// Bytes at the end of `typed` typed after a pause, before they were
    /// stable: the only typed text that may still be revised.
    tail: usize,
    /// Words in the cleaned text at the last commit.
    seen_words: usize,
    /// The case the field wants for the first word at the cursor (a sealed
    /// segment only): lower mid-sentence, upper at a sentence start.
    field_case: Case,
    /// The case its first word gets, decided by [`Self::match_case`].
    case: Case,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Case {
    /// As the model wrote it.
    #[default]
    Keep,
    Lower,
    Upper,
}

impl Segment {
    #[cfg(test)]
    fn delta(&self, cleaned: &str) -> Delta {
        self.plan(cleaned, false)
    }

    /// The edit that brings the segment to `cleaned`'s stable prefix (minus
    /// its last [`HOLDBACK_WORDS`] words), or to all of `cleaned` when
    /// `flush`. Only the tail may be deleted.
    fn plan(&self, cleaned: &str, flush: bool) -> Delta {
        let target = if flush {
            cleaned.trim_end()
        } else {
            stable_prefix(cleaned, HOLDBACK_WORDS)
        };
        let wanted = self.with_lead(skip_words(target, self.start_words));
        match next_delta(&self.typed, &wanted) {
            Delta::Diverged if self.tail > 0 => {
                let fixed = self.typed.len() - self.tail;
                let keep = match plan_finish(&self.typed, &wanted) {
                    FinishPlan::Replace { keep } => keep,
                    FinishPlan::TypeRest(_) => self.typed.len(),
                };
                match backspace_count(&self.typed[keep..]) {
                    Some(backspaces) if keep >= fixed => Delta::Revise {
                        keep,
                        backspaces,
                        insert: wanted[keep..].to_string(),
                    },
                    _ => Delta::Diverged,
                }
            }
            delta => delta,
        }
    }

    /// Records a typed edit. Text typed on a flush is tail; a commit's edit
    /// reaches its stable prefix, so nothing typed is tentative any more.
    fn apply(&mut self, edit: Delta, flush: bool) {
        let insert = match edit {
            Delta::Append(insert) => insert,
            Delta::Revise { keep, insert, .. } => {
                let fixed = self.typed.len() - self.tail;
                self.typed.truncate(keep);
                self.tail = keep - fixed;
                insert
            }
            Delta::Nothing | Delta::Diverged => return,
        };
        self.typed.push_str(&insert);
        self.tail = if flush { self.tail + insert.len() } else { 0 };
    }

    fn with_lead(&self, text: &str) -> String {
        let text = with_lead(self.lead, text);
        match self.case {
            Case::Keep => text,
            Case::Lower => decapitalize(&text),
            Case::Upper => capitalize(&text),
        }
    }

    /// Decides [`Self::case`] from `cleaned` until the segment has typed
    /// anything: after that, its text must stay as typed. A sentence start in
    /// the field is always capitalized. Mid-sentence, only a capital the model
    /// wrote after a sentence break (say, a pause to select) is lowered, and
    /// words in `custom_words` (names, mostly) keep it.
    fn match_case(&mut self, cleaned: &str, custom_words: &[String]) {
        if self.typed.is_empty() {
            let first = skip_words(cleaned, self.start_words)
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_matches(|c: char| !c.is_alphanumeric());
            let custom = custom_words.iter().any(|w| {
                w.split_whitespace()
                    .next()
                    .is_some_and(|w| w.to_lowercase() == first.to_lowercase())
            });
            self.case = match self.field_case {
                Case::Lower if sentence_break_before(cleaned, self.start_words) && !custom => {
                    Case::Lower
                }
                Case::Upper => Case::Upper,
                _ => Case::Keep,
            };
        }
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
            tail: 0,
            seen_words: self.seen_words,
            ..Default::default()
        };
        next - typed_end
    }
}

/// Word `n` of `cleaned` follows sentence-ending punctuation.
fn sentence_break_before(cleaned: &str, n: usize) -> bool {
    n > 0
        && cleaned[..word_offset(cleaned, n)]
            .trim_end()
            .ends_with(['.', '!', '?', '…'])
}

/// The case the field wants at the cursor: upper at the start of the field,
/// a line or a sentence; lower mid-sentence; unknown when unreadable.
fn field_case(around: Option<&Around>) -> Case {
    let Some((before, _)) = around else {
        return Case::Keep;
    };
    match before.trim_end_matches([' ', '\t']).chars().next_back() {
        None | Some('.' | '!' | '?' | '…' | '\n' | '\r' | '\u{2028}' | '\u{2029}') => Case::Upper,
        Some(_) => Case::Lower,
    }
}

/// Uppercases the first letter of `text`.
fn capitalize(text: &str) -> String {
    let start = text.len() - text.trim_start().len();
    match text[start..].chars().next() {
        Some(first) if first.is_lowercase() => {
            let rest = &text[start + first.len_utf8()..];
            format!("{}{}{rest}", &text[..start], first.to_uppercase())
        }
        _ => text.to_string(),
    }
}

/// Lowercases the first word of `text` when it is plainly capitalized
/// ("Che", "È", not "CI", "GitHub" or English "I", "I'm").
fn decapitalize(text: &str) -> String {
    let start = text.len() - text.trim_start().len();
    let word = text[start..].split_whitespace().next().unwrap_or("");
    let english_i = word == "I" || word.starts_with("I'") || word.starts_with("I\u{2019}");
    let mut chars = word.chars();
    match chars.next() {
        Some(first) if first.is_uppercase() && !english_i && !chars.any(char::is_uppercase) => {
            let rest = &text[start + first.len_utf8()..];
            format!("{}{}{rest}", &text[..start], first.to_lowercase())
        }
        _ => text.to_string(),
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

/// Splits text read around the cursor at the cursor's UTF-16 offset in it.
fn split_at_cursor((text, cursor): (String, usize)) -> Around {
    let units: Vec<u16> = text.encode_utf16().collect();
    let cursor = cursor.min(units.len());
    (
        String::from_utf16_lossy(&units[..cursor]),
        String::from_utf16_lossy(&units[cursor..]),
    )
}

/// What auto-learn needs about a segment the user edited: its typed text,
/// the field before the edit (`was`, holding it right before the cursor) and
/// the field once the edit was found (`now`), before anything else is typed,
/// so words dictated afterwards never look like part of the correction.
fn edited_segment(was: Option<Around>, typed: &str, now: Option<&Around>) -> Option<EditedSegment> {
    let (before, after) = was?;
    let (now_before, now_after) = now?;
    (!typed.trim().is_empty() && before.ends_with(typed)).then(|| {
        (
            typed.to_string(),
            before + &after,
            format!("{now_before}{now_after}"),
        )
    })
}

/// From the text around the cursor: whether the cursor is at the end of the
/// text (only whitespace after it), and the lead a new segment needs ("" after
/// whitespace or at the start). Unreadable: drop the tail, no lead.
fn cursor_context(around: Option<&Around>) -> (bool, &'static str) {
    let Some((before, after)) = around else {
        return (false, "");
    };
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
        // A segment sealed on a selection: its words replace the selection.
        (FieldCheck::Selection, FinishPlan::TypeRest(rest)) if typed.is_empty() => {
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
            typer.on_committed(generation, "hi", "", &OutputLanguageEvidence::Unknown, &[]);
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

    fn ctx(text: &str, cursor: usize) -> (bool, &'static str) {
        cursor_context(Some(&split_at_cursor((text.to_string(), cursor))))
    }

    #[test]
    fn edited_segment_is_learnable_without_later_dictation() {
        let around = |b: &str, a: &str| (b.to_string(), a.to_string());
        let typed = "Dear Andy send it";
        let (inserted, before, after) = edited_segment(
            Some(around("Hi. Dear Andy send it", "")),
            typed,
            Some(&around("Hi. Dear Handy", " send it")),
        )
        .unwrap();
        assert_eq!(after, "Hi. Dear Handy send it");
        assert_eq!(
            crate::correction_learner::learn(&inserted, &before, &after, &[]).pairs,
            vec![("Andy".to_string(), "Handy".to_string())]
        );
        let now = around("x", "");
        assert_eq!(
            edited_segment(Some(around("send it", "")), "it to", Some(&now)),
            None
        );
        assert_eq!(
            edited_segment(Some(around("send", "")), "", Some(&now)),
            None
        );
        assert_eq!(edited_segment(None, "send", Some(&now)), None);
        assert_eq!(edited_segment(Some(around("send", "")), "send", None), None);
    }

    /// A segment fed the cleaned commits in `commits`, typing every delta.
    fn typed_through(seg: &mut Segment, commits: &[&str]) {
        for cleaned in commits {
            let delta = seg.delta(cleaned);
            seg.apply(delta, false);
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
        let (carry, lead) = ctx("Dear Rob send it", 8);
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
        let (carry, lead) = ctx("send it Friday\n", 14);
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
    fn words_after_a_mid_sentence_seal_drop_the_models_capital() {
        let mut seg = Segment::default();
        typed_through(&mut seg, &["Ho comprato il pane e il latte oggi"]);
        // The user selected "latte" and paused: the model closed the sentence.
        seg.seal(false, "");
        seg.field_case = field_case(Some(&("Ho comprato il pane e il ".into(), "latte".into())));
        assert_eq!(seg.field_case, Case::Lower);
        let cleaned = "Ho comprato il pane e il latte oggi. Burro e marmellata stamattina";
        seg.match_case(cleaned, &[]);
        assert_eq!(seg.case, Case::Lower);
        assert_eq!(seg.with_lead(" Burro e"), "burro e");
        // A custom word (a name) keeps its capital.
        let mut named = seg.clone();
        named.match_case("oggi. Handy app ok ok", &["Handy".into()]);
        assert_eq!(named.case, Case::Keep);
        // Once something is typed the choice stays, so the text never diverges.
        seg.apply(Delta::Append("burro".into()), false);
        seg.match_case("oggi Burro e marmellata", &[]);
        assert_eq!(seg.case, Case::Lower);
        // No sentence break in the transcript: a capital there is a name.
        let mut fresh = Segment {
            field_case: Case::Lower,
            start_words: 2,
            ..Default::default()
        };
        fresh.match_case("send it Rob please now", &[]);
        assert_eq!(fresh.case, Case::Keep);
        // At a sentence start the model's lowercase continuation is raised.
        let mut start = Segment {
            field_case: Case::Upper,
            start_words: 2,
            ..Default::default()
        };
        start.match_case("send it poi domani", &[]);
        assert_eq!(start.case, Case::Upper);
        assert_eq!(start.with_lead("poi domani"), "Poi domani");
    }

    #[test]
    fn case_helpers() {
        let around = |b: &str| Some((b.to_string(), String::new()));
        assert_eq!(field_case(around("il ").as_ref()), Case::Lower);
        assert_eq!(field_case(around("fine. ").as_ref()), Case::Upper);
        assert_eq!(field_case(around("riga\n").as_ref()), Case::Upper);
        assert_eq!(field_case(around("").as_ref()), Case::Upper);
        assert_eq!(field_case(None), Case::Keep);
        assert_eq!(capitalize(" èccolo qui"), " Èccolo qui");
        assert_eq!(capitalize("GitHub"), "GitHub");
        assert!(sentence_break_before("a b. C", 2));
        assert!(!sentence_break_before("a b C", 2));
        assert!(!sentence_break_before("A b", 0));
        assert_eq!(decapitalize(" Che bello"), " che bello");
        assert_eq!(decapitalize("È vero"), "è vero");
        assert_eq!(decapitalize("A presto"), "a presto");
        for keep in ["I think", "I'm here", "CI pipeline", "GitHub", ""] {
            assert_eq!(decapitalize(keep), keep);
        }
    }

    #[test]
    fn cursor_context_reads_lead_and_end() {
        assert_eq!(cursor_context(None), (false, ""));
        assert_eq!(ctx("", 0), (true, ""));
        assert_eq!(ctx("hi ", 3), (true, ""));
        assert_eq!(ctx("hi there", 2), (false, " "));
        // UTF-16 offsets: the emoji is two units.
        assert_eq!(ctx("😀x", 2), (false, " "));
        assert_eq!(ctx("x", 99), (true, " "));
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
        // A segment sealed on a selection types its words over it.
        assert_eq!(
            plan_segment_finish(FieldCheck::Selection, "", "Rob", String::new()),
            FinishStep::Type {
                backspaces: 0,
                insert: "Rob".into()
            }
        );
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

    #[test]
    fn pause_types_the_held_back_words_as_a_revisable_tail() {
        let mut seg = Segment::default();
        typed_through(&mut seg, &["send it to Bob on Thursday"]);
        assert_eq!(seg.typed, "send it to");
        // A pause types the held-back words (and tentative ones) as tail.
        let flush = seg.plan("send it to Bob on Thursday at", true);
        assert_eq!(flush, Delta::Append(" Bob on Thursday at".into()));
        seg.apply(flush, true);
        assert_eq!(seg.typed, "send it to Bob on Thursday at");
        assert_eq!(seg.tail, " Bob on Thursday at".len());

        // Until the stable prefix reaches a revised word, nothing changes.
        assert_eq!(
            seg.plan("send it to Bob on Friday at noon", false),
            Delta::Nothing
        );
        // Then only the revised part of the tail is deleted.
        let revise = seg.plan("send it to Bob on Friday at noon today ok", false);
        assert_eq!(
            revise,
            Delta::Revise {
                keep: "send it to Bob on ".len(),
                backspaces: "Thursday at".len(),
                insert: "Friday at".into()
            }
        );
        seg.apply(revise, false);
        assert_eq!(seg.typed, "send it to Bob on Friday at");
        assert_eq!(seg.tail, 0, "a commit's text is stable");
        // Unchanged words just continue.
        assert_eq!(
            seg.plan("send it to Bob on Friday at noon today ok go", false),
            Delta::Append(" noon".into())
        );
    }

    #[test]
    fn text_typed_before_the_tail_is_never_revised() {
        let mut seg = Segment::default();
        typed_through(&mut seg, &["send it to Bob on Thursday"]);
        let flush = seg.plan("send it to Bob on Thursday", true);
        seg.apply(flush, true);
        // "to" was typed from the stable prefix: a change there is a divergence.
        assert_eq!(
            seg.plan("send it for Bob on Thursday at noon", false),
            Delta::Diverged
        );
        // A flush with fewer words deletes nothing.
        assert_eq!(seg.plan("send it to Bob", true), Delta::Nothing);
    }
}

//! Windows backend of [`crate::accessibility`]: the focused text field through
//! UI Automation (UIA), with the same API as the macOS backend.
//!
//! UIA has no UTF-16 offsets. Text is read relative to the caret by moving
//! `TextUnit_Character` (a UTF-16 unit, a code point or a grapheme, depending
//! on the app), then trimmed to exact UTF-16 counts here: moving N characters
//! always covers at least N UTF-16 units, so the trim is exact.
//!
//! Type-checked only: not yet run on Windows.

use super::{decode_utf16_window, Utf16Range, LARGE_DOC_UTF16, WINDOW_RADIUS_UTF16};
use std::cell::RefCell;
use windows::core::{Interface, BSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationElement,
    IUIAutomationTextPattern, IUIAutomationTextRange, IUIAutomationValuePattern,
    TextPatternRangeEndpoint_End, TextPatternRangeEndpoint_Start, TextUnit_Character,
    UIA_ComboBoxControlTypeId, UIA_DocumentControlTypeId, UIA_EditControlTypeId,
    UIA_GroupControlTypeId, UIA_IsReadOnlyAttributeId, UIA_TextPatternId, UIA_ValuePatternId,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

/// Per-call ceilings, like the Mac's 0.2 s AX messaging timeout: a hung app
/// must not stall the typer thread for UIA's default 20 s.
const CONNECTION_TIMEOUT_MS: u32 = 200;
const TRANSACTION_TIMEOUT_MS: u32 = 500;

thread_local! {
    static AUTOMATION: RefCell<Option<IUIAutomation>> = const { RefCell::new(None) };
}

/// This thread's UIA client. Joins COM as MTA first and never leaves it (the
/// thread may use UIA again); a thread already in an STA keeps it, and UIA
/// works there too. Never call from the Tauri main thread: a UIA call into
/// Handy's own window would wait on the thread that has to answer it.
fn automation() -> Option<IUIAutomation> {
    AUTOMATION.with(|cell| {
        let mut cell = cell.borrow_mut();
        if cell.is_none() {
            // SAFETY: no preconditions. S_FALSE (already joined) and
            // RPC_E_CHANGED_MODE (already in an STA) both leave COM usable.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            *cell = create_automation();
        }
        cell.clone()
    })
}

fn create_automation() -> Option<IUIAutomation> {
    // SAFETY: COM activation of the system UIA client on a COM thread.
    let modern: windows::core::Result<IUIAutomation2> =
        unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) };
    if let Ok(automation) = modern {
        // SAFETY: setters on a live client; a failure keeps the defaults.
        unsafe {
            let _ = automation.SetConnectionTimeout(CONNECTION_TIMEOUT_MS);
            let _ = automation.SetTransactionTimeout(TRANSACTION_TIMEOUT_MS);
        }
        return automation.cast().ok();
    }
    // Before Windows 8: no timeouts.
    // SAFETY: as above.
    unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }.ok()
}

/// Handle to the focused UIA element. Clone = AddRef, Drop = Release.
#[derive(Clone)]
pub struct FocusedField {
    pid: i32,
    element: IUIAutomationElement,
}

// SAFETY: UIA client objects are free-threaded (CUIAutomation is registered
// "Both"), so an element may be used from any COM thread; every method joins
// COM on its calling thread through `automation()` before touching it.
unsafe impl Send for FocusedField {}

pub fn focused_field() -> Option<FocusedField> {
    let automation = automation()?;
    // SAFETY: COM calls on a live client; errors map to None.
    let element = unsafe { automation.GetFocusedElement() }.ok()?;
    let pid = unsafe { element.CurrentProcessId() }.ok()?;
    Some(FocusedField { pid, element })
}

/// Nothing to switch on: Chromium/Electron build their UIA tree on the first
/// UIA query, which `wait_for_focused_field` then polls for.
pub fn enable_manual_accessibility(_pid: i32) {}

/// Pid of the foreground window's process.
pub(super) fn focused_app_pid() -> Option<i32> {
    let mut pid = 0u32;
    // SAFETY: `pid` is a valid out-pointer for the duration of the call.
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid)) };
    (pid != 0).then_some(pid as i32)
}

impl FocusedField {
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// The AX role this element would have on macOS, so live typing's role
    /// checks stay platform-neutral: editable text is "AXTextArea" (or
    /// "AXComboBox"), a read-only document such as a Chromium page still
    /// building its tree is "AXWebArea".
    #[allow(non_upper_case_globals)] // the windows crate's UIA constant names
    pub fn role(&self) -> Option<String> {
        automation()?;
        // SAFETY: COM call on a live element.
        let control = unsafe { self.element.CurrentControlType() }.ok()?;
        let editable = || self.text_pattern().is_some() && !self.is_read_only();
        let role = match control {
            UIA_ComboBoxControlTypeId => "AXComboBox",
            UIA_EditControlTypeId => "AXTextArea",
            _ if editable() => "AXTextArea",
            UIA_DocumentControlTypeId => "AXWebArea",
            UIA_GroupControlTypeId => "AXGroup",
            other => return Some(format!("UIA{}", other.0)),
        };
        Some(role.to_string())
    }

    /// The UI framework ("Win32", "WPF", "XAML", "Chrome", ...), for logs.
    pub fn subrole(&self) -> Option<String> {
        automation()?;
        // SAFETY: COM call on a live element.
        let framework = unsafe { self.element.CurrentFrameworkId() }.ok()?;
        Some(framework.to_string())
    }

    /// The whole text up to 10_000 UTF-16 units; past that, ~±2_000 units
    /// around the cursor.
    pub fn value(&self) -> Option<String> {
        automation()?;
        let Some(text) = self.text_pattern() else {
            // A ValuePattern-only control: its value, with no caret to window on.
            let value = self.value_pattern()?;
            // SAFETY: COM call on a live pattern.
            return unsafe { value.CurrentValue() }.ok().map(|v| v.to_string());
        };
        // SAFETY: COM call on a live pattern.
        let document = unsafe { text.DocumentRange() }.ok()?;
        let units = range_units(&document, Some(LARGE_DOC_UTF16 + 1))?;
        if units.len() <= LARGE_DOC_UTF16 {
            return Some(decode_utf16_window(&units));
        }
        self.text_near_cursor().map(|(text, _)| text)
    }

    /// The selection, as a length and an offset from the document start. The
    /// offset is counted up to 10_000 units only and saturates there; callers
    /// only rely on the length.
    pub fn selected_range(&self) -> Option<Utf16Range> {
        automation()?;
        let text = self.text_pattern()?;
        let selection = selection(&text)?;
        let length = range_units(&selection, Some(LARGE_DOC_UTF16 + 1))?.len();
        // SAFETY: COM calls on a live pattern and ranges.
        let prefix = unsafe { text.DocumentRange() }.ok()?;
        unsafe {
            prefix
                .MoveEndpointByRange(
                    TextPatternRangeEndpoint_End,
                    &selection,
                    TextPatternRangeEndpoint_Start,
                )
                .ok()?;
        }
        let location = range_units(&prefix, Some(LARGE_DOC_UTF16 + 1))?.len();
        Some(Utf16Range { location, length })
    }

    /// Up to `max_utf16` UTF-16 units immediately before the cursor (the
    /// selection start). None if unreadable.
    pub fn text_before_cursor(&self, max_utf16: usize) -> Option<String> {
        automation()?;
        let range = self.collapsed_at_cursor()?;
        // SAFETY: COM call on a live range.
        unsafe {
            range
                .MoveEndpointByUnit(
                    TextPatternRangeEndpoint_Start,
                    TextUnit_Character,
                    -unit_count(max_utf16),
                )
                .ok()?;
        }
        let units = range_units(&range, None)?;
        Some(decode_utf16_window(
            &units[units.len().saturating_sub(max_utf16)..],
        ))
    }

    /// ~±2_000 UTF-16 units around the cursor and the cursor's offset in them;
    /// only the part before it when the rest is unreadable.
    pub fn text_near_cursor(&self) -> Option<(String, usize)> {
        let before = self.text_before_cursor(WINDOW_RADIUS_UTF16)?;
        let cursor = before.encode_utf16().count();
        let after = self
            .text_after_cursor(WINDOW_RADIUS_UTF16)
            .unwrap_or_default();
        Some((before + &after, cursor))
    }

    /// Password fields, and any element whose state can't be read: an
    /// unreadable field is never treated as safe to read.
    pub fn is_secure(&self) -> bool {
        if automation().is_none() {
            return true;
        }
        // SAFETY: COM call on a live element.
        unsafe { self.element.CurrentIsPassword() }.map_or(true, |p| p.as_bool())
    }

    pub fn same_as(&self, other: &FocusedField) -> bool {
        automation()
            // SAFETY: COM call on a live client with two live elements.
            .and_then(|a| unsafe { a.CompareElements(&self.element, &other.element) }.ok())
            .is_some_and(|same| same.as_bool())
    }

    /// The document's length, known only up to 10_000 units: UIA has no cheap
    /// total, so a larger document reports None.
    pub(super) fn number_of_characters(&self) -> Option<usize> {
        automation()?;
        let text = self.text_pattern()?;
        // SAFETY: COM call on a live pattern.
        let document = unsafe { text.DocumentRange() }.ok()?;
        let len = range_units(&document, Some(LARGE_DOC_UTF16 + 1))?.len();
        (len <= LARGE_DOC_UTF16).then_some(len)
    }

    fn text_after_cursor(&self, max_utf16: usize) -> Option<String> {
        let range = self.collapsed_at_cursor()?;
        // SAFETY: COM call on a live range.
        unsafe {
            range
                .MoveEndpointByUnit(
                    TextPatternRangeEndpoint_End,
                    TextUnit_Character,
                    unit_count(max_utf16),
                )
                .ok()?;
        }
        let units = range_units(&range, None)?;
        Some(decode_utf16_window(&units[..units.len().min(max_utf16)]))
    }

    /// An empty range at the selection start.
    fn collapsed_at_cursor(&self) -> Option<IUIAutomationTextRange> {
        let text = self.text_pattern()?;
        // SAFETY: COM calls on a live pattern and ranges.
        let range = unsafe { selection(&text)?.Clone() }.ok()?;
        unsafe {
            range
                .MoveEndpointByRange(
                    TextPatternRangeEndpoint_End,
                    &range,
                    TextPatternRangeEndpoint_Start,
                )
                .ok()?;
        }
        Some(range)
    }

    fn text_pattern(&self) -> Option<IUIAutomationTextPattern> {
        // SAFETY: COM call on a live element; an unsupported pattern is an error.
        unsafe { self.element.GetCurrentPatternAs(UIA_TextPatternId) }.ok()
    }

    fn value_pattern(&self) -> Option<IUIAutomationValuePattern> {
        // SAFETY: as `text_pattern`.
        unsafe { self.element.GetCurrentPatternAs(UIA_ValuePatternId) }.ok()
    }

    /// ValuePattern's IsReadOnly, else the document's IsReadOnly text
    /// attribute; editable when neither answers.
    fn is_read_only(&self) -> bool {
        if let Some(value) = self.value_pattern() {
            // SAFETY: COM call on a live pattern.
            if let Ok(read_only) = unsafe { value.CurrentIsReadOnly() } {
                return read_only.as_bool();
            }
        }
        let Some(text) = self.text_pattern() else {
            return false;
        };
        // SAFETY: COM calls on a live pattern and range. A mixed or
        // unsupported attribute comes back as a non-bool VARIANT.
        unsafe { text.DocumentRange() }
            .and_then(|document| unsafe { document.GetAttributeValue(UIA_IsReadOnlyAttributeId) })
            .ok()
            .and_then(|v| bool::try_from(&v).ok())
            .unwrap_or(false)
    }
}

/// The first selected range: the caret when nothing is selected.
fn selection(text: &IUIAutomationTextPattern) -> Option<IUIAutomationTextRange> {
    // SAFETY: COM calls on a live pattern and array.
    let ranges = unsafe { text.GetSelection() }.ok()?;
    if unsafe { ranges.Length() }.ok()? < 1 {
        return None;
    }
    unsafe { ranges.GetElement(0) }.ok()
}

/// The range's text as UTF-16 units, at most `max` of them.
fn range_units(range: &IUIAutomationTextRange, max: Option<usize>) -> Option<Vec<u16>> {
    let max = max.map_or(-1, unit_count);
    // SAFETY: COM call on a live range.
    let text: BSTR = unsafe { range.GetText(max) }.ok()?;
    Some(text.to_vec())
}

fn unit_count(units: usize) -> i32 {
    i32::try_from(units).unwrap_or(i32::MAX)
}

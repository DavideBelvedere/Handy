//! Read-only access to the focused text field through the macOS Accessibility
//! (AX) API, for auto-learn and live typing. Uses the Accessibility permission
//! Handy already requests. Other platforms get no-op stubs with the same API.
//!
//! AX offsets (`AXSelectedTextRange`, `AXStringForRange`) are UTF-16 code
//! units: text is always handled as UTF-16 units, never byte-sliced.

use log::info;
use serde::Serialize;
use specta::Type;
use std::time::Duration;

/// UTF-16 code-unit range, as AX reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub struct Utf16Range {
    pub location: usize,
    pub length: usize,
}

/// Above this many UTF-16 units, `value()` reads a window around the cursor
/// instead of copying the whole document.
const LARGE_DOC_UTF16: usize = 10_000;
const WINDOW_RADIUS_UTF16: usize = 2_000;

pub use imp::{enable_manual_accessibility, focused_field, FocusedField};

/// Sets AXManualAccessibility on the app owning keyboard focus, so a
/// Chrome/Electron app has built its AX tree by the time it is read.
pub fn enable_manual_accessibility_for_focused_app() {
    if let Some(pid) = imp::focused_app_pid() {
        enable_manual_accessibility(pid);
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{
        cursor_window, decode_utf16_window, is_large_doc, range_before, Utf16Range,
        WINDOW_RADIUS_UTF16,
    };
    use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
    use objc2_core_foundation::{
        CFBoolean, CFIndex, CFNumber, CFRange, CFRetained, CFString, CFType,
    };
    use std::ptr::{self, NonNull};

    const MESSAGING_TIMEOUT_SECS: f32 = 0.2;

    /// Retained handle to the focused AX element. Clone = CFRetain, Drop =
    /// CFRelease (both via `CFRetained`).
    #[derive(Clone)]
    pub struct FocusedField {
        pid: i32,
        element: CFRetained<AXUIElement>,
    }

    // SAFETY: an AXUIElementRef is an immutable CF token (pid + element id);
    // CFRetain/CFRelease are atomic, and AX calls are Mach IPC that never
    // mutate the handle (the messaging timeout is set once, before the field
    // is handed out). Moving it to another thread is therefore sound.
    unsafe impl Send for FocusedField {}

    pub fn focused_field() -> Option<FocusedField> {
        // SAFETY: AXUIElementCreateSystemWide has no preconditions and returns
        // a +1 reference, which CFRetained owns.
        let system = unsafe { AXUIElement::new_system_wide() };
        set_timeout(&system);
        let element = copy_attr(&system, "AXFocusedUIElement")?
            .downcast::<AXUIElement>()
            .ok()?;
        set_timeout(&element);
        let mut pid: libc::pid_t = 0;
        // SAFETY: `pid` is a valid out-pointer for the duration of the call.
        let err = unsafe { element.pid(NonNull::from(&mut pid)) };
        (err == AXError::Success).then_some(FocusedField { pid, element })
    }

    /// Asks Chrome/Electron apps to build their AX tree. Errors are ignored:
    /// native apps don't know this attribute and are readable anyway.
    pub fn enable_manual_accessibility(pid: i32) {
        // SAFETY: AXUIElementCreateApplication accepts any pid and returns a
        // +1 reference, which CFRetained owns.
        let app = unsafe { AXUIElement::new_application(pid) };
        set_timeout(&app);
        let attr = CFString::from_static_str("AXManualAccessibility");
        // SAFETY: `attr` and the boolean are valid CF objects; AX retains what
        // it needs.
        let _ = unsafe { app.set_attribute_value(&attr, CFBoolean::new(true)) };
    }

    /// Pid of the application owning keyboard focus, readable even before
    /// `enable_manual_accessibility` builds a Chrome/Electron AX tree.
    pub(super) fn focused_app_pid() -> Option<i32> {
        // SAFETY: see `focused_field`.
        let system = unsafe { AXUIElement::new_system_wide() };
        set_timeout(&system);
        let app = copy_attr(&system, "AXFocusedApplication")?
            .downcast::<AXUIElement>()
            .ok()?;
        set_timeout(&app);
        let mut pid: libc::pid_t = 0;
        // SAFETY: `pid` is a valid out-pointer for the duration of the call.
        let err = unsafe { app.pid(NonNull::from(&mut pid)) };
        (err == AXError::Success).then_some(pid)
    }

    impl FocusedField {
        pub fn pid(&self) -> i32 {
            self.pid
        }

        pub fn role(&self) -> Option<String> {
            self.string_attr("AXRole")
        }

        pub fn subrole(&self) -> Option<String> {
            self.string_attr("AXSubrole")
        }

        /// Full AXValue; when AXNumberOfCharacters > 10_000, instead a window
        /// of ~±2_000 UTF-16 units around the cursor via AXStringForRange (no
        /// surrogate pair split at its edges).
        pub fn value(&self) -> Option<String> {
            match self.number_of_characters() {
                // Never copy a huge AXValue: no cursor or unreadable window -> None.
                Some(total) if is_large_doc(total) => {
                    let cursor = self.selected_range()?.location;
                    self.string_for_range(cursor_window(cursor, total))
                }
                _ => self.string_attr("AXValue"),
            }
        }

        pub fn selected_range(&self) -> Option<Utf16Range> {
            let value = copy_attr(&self.element, "AXSelectedTextRange")?
                .downcast::<AXValue>()
                .ok()?;
            let mut range = CFRange {
                location: 0,
                length: 0,
            };
            // SAFETY: `range` is a CFRange, matching the requested type, and
            // outlives the call. AXValueGetValue returns false on type mismatch.
            let ok = unsafe { value.value(AXValueType::CFRange, NonNull::from(&mut range).cast()) };
            if !ok {
                return None;
            }
            // Negative values (kCFNotFound) mean "no cursor".
            Some(Utf16Range {
                location: usize::try_from(range.location).ok()?,
                length: usize::try_from(range.length).ok()?,
            })
        }

        /// Up to `max_utf16` UTF-16 units immediately before the cursor
        /// (AXStringForRange). None if unreadable.
        pub fn text_before_cursor(&self, max_utf16: usize) -> Option<String> {
            let cursor = self.selected_range()?.location;
            self.string_for_range(range_before(cursor, max_utf16))
        }

        /// ~±2_000 UTF-16 units around the cursor and the cursor's offset in
        /// them; only the part before it when the length is unknown.
        pub fn text_near_cursor(&self) -> Option<(String, usize)> {
            let cursor = self.selected_range()?.location;
            let Some(total) = self.number_of_characters() else {
                let before = self.text_before_cursor(WINDOW_RADIUS_UTF16)?;
                let len = before.encode_utf16().count();
                return Some((before, len));
            };
            let range = cursor_window(cursor, total);
            let text = self.string_for_range(range)?;
            Some((text, cursor.min(total) - range.location))
        }

        pub fn is_secure(&self) -> bool {
            const SECURE: &str = "AXSecureTextField";
            self.role().as_deref() == Some(SECURE)
                || self.subrole().as_deref() == Some(SECURE)
                || crate::secure_input::is_enabled_now()
        }

        pub fn same_as(&self, other: &FocusedField) -> bool {
            // CFRetained derefs to AXUIElement, whose PartialEq is CFEqual.
            *self.element == *other.element
        }

        pub(super) fn number_of_characters(&self) -> Option<usize> {
            let number = copy_attr(&self.element, "AXNumberOfCharacters")?
                .downcast::<CFNumber>()
                .ok()?;
            usize::try_from(number.as_i64()?).ok()
        }

        fn string_attr(&self, name: &'static str) -> Option<String> {
            let s = copy_attr(&self.element, name)?
                .downcast::<CFString>()
                .ok()?;
            Some(decode_utf16_window(&utf16_units(&s)))
        }

        fn string_for_range(&self, range: Utf16Range) -> Option<String> {
            let mut cf_range = CFRange {
                location: CFIndex::try_from(range.location).ok()?,
                length: CFIndex::try_from(range.length).ok()?,
            };
            // SAFETY: `cf_range` is a CFRange matching the declared type and
            // is only read during the call. Returns a +1 reference or None.
            let param =
                unsafe { AXValue::new(AXValueType::CFRange, NonNull::from(&mut cf_range).cast()) }?;
            let attr = CFString::from_static_str("AXStringForRange");
            let mut out: *const CFType = ptr::null();
            // SAFETY: `param` and `attr` are valid CF objects and `out` is a
            // valid out-pointer; ownership of the result is adopted below.
            let err = unsafe {
                self.element.copy_parameterized_attribute_value(
                    &attr,
                    &param,
                    NonNull::from(&mut out),
                )
            };
            let s = adopt(out, err)?.downcast::<CFString>().ok()?;
            Some(decode_utf16_window(&utf16_units(&s)))
        }
    }

    fn set_timeout(element: &AXUIElement) {
        // SAFETY: `element` is a valid AXUIElement; the call only stores the
        // timeout used for its future messages.
        let _ = unsafe { element.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
    }

    fn copy_attr(element: &AXUIElement, name: &'static str) -> Option<CFRetained<CFType>> {
        let attr = CFString::from_static_str(name);
        let mut out: *const CFType = ptr::null();
        // SAFETY: `attr` is a valid CFString and `out` a valid out-pointer;
        // ownership of the result is adopted below.
        let err = unsafe { element.copy_attribute_value(&attr, NonNull::from(&mut out)) };
        adopt(out, err)
    }

    /// Takes ownership of a Copy-rule out-parameter (released on drop even if
    /// AX reported an error), returning it only on success.
    fn adopt(out: *const CFType, err: AXError) -> Option<CFRetained<CFType>> {
        // SAFETY: AX Copy functions return a +1 reference in `out` (or leave
        // it null), so CFRetained takes over that single ownership.
        let value = NonNull::new(out.cast_mut()).map(|p| unsafe { CFRetained::from_raw(p) });
        if err == AXError::Success {
            value
        } else {
            None
        }
    }

    fn utf16_units(s: &CFString) -> Vec<u16> {
        let len = s.length();
        let mut units = vec![0u16; usize::try_from(len).unwrap_or(0)];
        // SAFETY: `units` holds exactly `len` UniChars and the range covers
        // the whole string, so CFStringGetCharacters stays in bounds.
        unsafe {
            s.characters(
                CFRange {
                    location: 0,
                    length: units.len() as CFIndex,
                },
                units.as_mut_ptr(),
            )
        };
        units
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::Utf16Range;

    /// Stub: Accessibility reads are macOS-only.
    #[derive(Clone)]
    pub struct FocusedField;

    pub fn focused_field() -> Option<FocusedField> {
        None
    }

    pub fn enable_manual_accessibility(_pid: i32) {}

    pub(super) fn focused_app_pid() -> Option<i32> {
        None
    }

    impl FocusedField {
        pub fn pid(&self) -> i32 {
            0
        }
        pub fn role(&self) -> Option<String> {
            None
        }
        pub fn subrole(&self) -> Option<String> {
            None
        }
        pub fn value(&self) -> Option<String> {
            None
        }
        pub fn selected_range(&self) -> Option<Utf16Range> {
            None
        }
        pub fn text_before_cursor(&self, _max_utf16: usize) -> Option<String> {
            None
        }
        pub fn text_near_cursor(&self) -> Option<(String, usize)> {
            None
        }
        pub fn is_secure(&self) -> bool {
            false
        }
        pub fn same_as(&self, _other: &FocusedField) -> bool {
            false
        }
        pub(super) fn number_of_characters(&self) -> Option<usize> {
            None
        }
    }
}

/// Above this size `value()` reads only a window around the cursor.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn is_large_doc(total_utf16: usize) -> bool {
    total_utf16 > LARGE_DOC_UTF16
}

/// ~±2_000 UTF-16 units around `cursor`, clamped to `[0, total]`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn cursor_window(cursor: usize, total: usize) -> Utf16Range {
    let cursor = cursor.min(total);
    let start = cursor.saturating_sub(WINDOW_RADIUS_UTF16);
    let end = cursor.saturating_add(WINDOW_RADIUS_UTF16).min(total);
    Utf16Range {
        location: start,
        length: end - start,
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn range_before(cursor: usize, max_utf16: usize) -> Utf16Range {
    let start = cursor.saturating_sub(max_utf16);
    Utf16Range {
        location: start,
        length: cursor - start,
    }
}

/// Decodes UTF-16 read from an arbitrary range, dropping a surrogate half cut
/// off at either edge so no pair is split into U+FFFD.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn decode_utf16_window(units: &[u16]) -> String {
    let is_high = |u: &u16| (0xD800..=0xDBFF).contains(u);
    let is_low = |u: &u16| (0xDC00..=0xDFFF).contains(u);
    let mut units = units;
    if units.first().is_some_and(is_low) {
        units = &units[1..];
    }
    if units.last().is_some_and(is_high) {
        units = &units[..units.len() - 1];
    }
    String::from_utf16_lossy(units)
}

/// Content-free snapshot of the focused field, for the AX probe gate.
#[derive(Debug, Serialize, Type)]
pub struct FocusProbe {
    app_pid: i32,
    role: Option<String>,
    subrole: Option<String>,
    value_len_utf16: Option<usize>,
    selected_range: Option<Utf16Range>,
    window_read_ok: bool,
    is_secure: bool,
}

/// Temporary diagnostic for the AX probe gate: after `delay_ms` (time to click
/// into another app), reports how readable the focused field is. Never
/// returns or logs text content.
#[tauri::command]
#[specta::specta]
pub async fn probe_focused_field(delay_ms: u32) -> Result<FocusProbe, String> {
    tauri::async_runtime::spawn_blocking(move || {
        std::thread::sleep(Duration::from_millis(delay_ms.into()));
        if let Some(pid) = imp::focused_app_pid() {
            enable_manual_accessibility(pid);
            // Chrome/Electron build their AX tree asynchronously after the flag.
            std::thread::sleep(Duration::from_millis(200));
        }
        let field = focused_field().ok_or("No focused element readable via Accessibility")?;
        let probe = FocusProbe {
            app_pid: field.pid(),
            role: field.role(),
            subrole: field.subrole(),
            // Total document length, not value()'s window, so large docs show as such.
            value_len_utf16: field
                .number_of_characters()
                .or_else(|| field.value().map(|v| v.encode_utf16().count())),
            selected_range: field.selected_range(),
            window_read_ok: field.text_before_cursor(32).is_some(),
            is_secure: field.is_secure(),
        };
        info!("AX probe: {probe:?}");
        Ok(probe)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    /// Reads `range` out of `doc` the way AXStringForRange would.
    fn read(doc: &[u16], range: Utf16Range) -> String {
        decode_utf16_window(&doc[range.location..range.location + range.length])
    }

    #[test]
    fn focused_field_is_send_and_clone() {
        fn assert_send_clone<T: Send + Clone>() {}
        assert_send_clone::<FocusedField>();
    }

    #[test]
    fn only_docs_over_10k_units_are_windowed() {
        assert!(!is_large_doc(0));
        assert!(!is_large_doc(9_999));
        assert!(!is_large_doc(10_000));
        assert!(is_large_doc(10_001));
        assert!(is_large_doc(1_000_000));
    }

    #[test]
    fn cursor_window_centers_on_cursor() {
        assert_eq!(
            cursor_window(5_000, 10_001),
            Utf16Range {
                location: 3_000,
                length: 4_000
            }
        );
    }

    #[test]
    fn cursor_window_clamps_at_start_and_end() {
        let at_start = Utf16Range {
            location: 0,
            length: 2_000,
        };
        assert_eq!(cursor_window(0, 20_000), at_start);
        let at_end = Utf16Range {
            location: 18_000,
            length: 2_000,
        };
        assert_eq!(cursor_window(20_000, 20_000), at_end);
        // A cursor past the end (stale AX value) is clamped, never overflows.
        assert_eq!(cursor_window(99_999, 20_000), at_end);
        assert_eq!(cursor_window(usize::MAX, 20_000), at_end);
    }

    #[test]
    fn range_before_clamps_at_zero() {
        assert_eq!(
            range_before(10, 32),
            Utf16Range {
                location: 0,
                length: 10
            }
        );
        assert_eq!(
            range_before(100, 32),
            Utf16Range {
                location: 68,
                length: 32
            }
        );
        assert_eq!(
            range_before(0, 32),
            Utf16Range {
                location: 0,
                length: 0
            }
        );
    }

    #[test]
    fn decode_keeps_whole_surrogate_pairs() {
        assert_eq!(decode_utf16_window(&units("a😀b")), "a😀b");
        assert_eq!(decode_utf16_window(&units("😀")), "😀");
        assert_eq!(decode_utf16_window(&[]), "");
    }

    #[test]
    fn decode_drops_split_pair_at_window_start() {
        let doc = units("😀ab"); // [high, low, a, b]
        let s = decode_utf16_window(&doc[1..]);
        assert_eq!(s, "ab");
        assert!(!s.contains('\u{FFFD}'));
    }

    #[test]
    fn decode_drops_split_pair_at_window_end() {
        let doc = units("ab😀"); // [a, b, high, low]
        let s = decode_utf16_window(&doc[..3]);
        assert_eq!(s, "ab");
        assert!(!s.contains('\u{FFFD}'));
    }

    #[test]
    fn accents_count_as_utf16_units_not_chars() {
        // Precomposed é is 1 unit; e + U+0301 is 2 units.
        let doc = units("caf\u{e9} cafe\u{301}");
        assert_eq!(doc.len(), 10);
        let r = range_before(doc.len(), 5);
        assert_eq!(read(&doc, r), "cafe\u{301}");
        let r = range_before(4, 4);
        assert_eq!(read(&doc, r), "caf\u{e9}");
    }

    #[test]
    fn text_before_cursor_never_splits_emoji() {
        let doc = units("hi 👋🏽 there");
        // Cursor after "👋🏽" (2 + 2 units); a 3-unit read starts mid-pair.
        let cursor = units("hi 👋🏽").len();
        let s = read(&doc, range_before(cursor, 3));
        assert_eq!(s, "🏽");
    }

    #[test]
    fn large_emoji_doc_window_is_valid_text() {
        // 30k units of mixed emoji/accents: any window must decode cleanly.
        let doc: Vec<u16> = units(&"é😀x".repeat(7_500));
        assert_eq!(doc.len(), 30_000);
        for cursor in [0, 1, 2, 3, 4_001, 15_001, 29_999, 30_000] {
            let range = cursor_window(cursor, doc.len());
            assert!(range.length <= 2 * WINDOW_RADIUS_UTF16);
            let s = read(&doc, range);
            assert!(!s.contains('\u{FFFD}'), "split pair at cursor {cursor}");
            assert!(s.encode_utf16().count() + 2 >= range.length);
        }
    }

    /// Manual smoke test of real AX: prints role and lengths only (no content).
    /// Needs Accessibility permission for the test binary.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn smoke_focused_field() {
        // SAFETY: AXIsProcessTrusted has no preconditions.
        let trusted = unsafe { objc2_application_services::AXIsProcessTrusted() };
        println!("AXIsProcessTrusted={trusted}");
        match focused_field() {
            Some(f) => println!(
                "pid={} role={:?} subrole={:?} value_len={:?} chars={:?} sel={:?} before_ok={} secure={}",
                f.pid(),
                f.role(),
                f.subrole(),
                f.value().map(|v| v.encode_utf16().count()),
                f.number_of_characters(),
                f.selected_range(),
                f.text_before_cursor(32).is_some(),
                f.is_secure()
            ),
            None => println!("focused_field() = None (no AX permission or no focus)"),
        }
    }
}

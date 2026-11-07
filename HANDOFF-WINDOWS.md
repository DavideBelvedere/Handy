# Handoff: continuing on Windows

Written 2026-11-07 on the Mac, for the agent that picks this fork up on a Windows PC.
The user (Davide) speaks Italian; answer in Italian. Delete this file once Windows works.

## Where things stand

Fork `DavideBelvedere/Handy`, branch `feat/auto-learn-live-typing-backtrack` (upstream:
`cjpais/Handy`). The fork adds, on top of upstream: live typing (text typed into the focused
field while speaking), auto-learn (word replacements learned from the user's own fixes),
names around the cursor sent to the model as vocabulary, offline "scratch that" /
value corrections, word replacements, and a backtrack LLM prompt.

Live typing, auto-learn and window-context vocabulary all read the focused field through
one module, `src-tauri/src/accessibility.rs`. On macOS that is the AX API. **On Windows it is
the new `src-tauri/src/accessibility_uia.rs` (UI Automation), written on the Mac and only
type-checked (`cargo check` + `clippy -D warnings` for `x86_64-pc-windows-msvc` in a scratch
crate). It has never run.** Testing and fixing it is the job here.

Also done in the same session, working on macOS:

- **Silence stop:** a recording with no speech (VAD) for 60 s stops as if the shortcut were
  pressed again. `SILENCE_STOP` / `SilenceWatch` in `src-tauri/src/managers/audio.rs`. Only
  fires with VAD on. No UI setting, no toast.
- **Copy-only finish:** with Post Process off, live typing never rewrites the field at the end;
  the whole dictation goes to the clipboard. Only exception: if the field is exactly as live
  typing left it, the words still held back are appended (`finish_copy_only` in
  `live_typing.rs`). Auto-submit is not sent in this mode.
- **Stable macOS signing:** `scripts/install-macos.sh` signs with the "Handy Local"
  self-signed certificate when present, so the Accessibility grant survives rebuilds.

## Setting up the Windows PC (once)

From `BUILD.md`: Rust, Bun, Visual Studio Build Tools with C++,
`winget install Kitware.CMake`, `winget install KhronosGroup.VulkanSDK` (then a new terminal).

```powershell
git clone https://github.com/DavideBelvedere/Handy.git
cd Handy
git checkout feat/auto-learn-live-typing-backtrack
bun install
bun run tauri dev
```

- `bun run tauri build` fails at the end (`program not found`): the release `signCommand` only
  exists in upstream CI. Use `bun run tauri build --no-bundle` for a release exe.
- Path-limit errors (`MSB3491`, `FTK1011`): `$env:CARGO_TARGET_DIR = "C:\h"`.
- First check: `cd src-tauri; cargo check --all-targets; cargo test --lib` (450 tests passed
  on the Mac). Fix any Windows-only compile errors first: the full crate was never built for
  Windows, only `accessibility_uia.rs` in isolation.

## What to test on Windows

Prerequisites for live typing: toggle shortcut mode, a streaming model (the user uses
`nemotron-3.5-asr-streaming-0.6b`), Paste Method not "None", Live Typing on (Advanced >
Experimental; enable "Experimental features" first). Post Process is off by default on a
fresh install; Apple Intelligence does not exist on Windows, so any LLM feature needs an API
provider.

Test in this order, each in Notepad, a Chrome `<textarea>`, a Chrome contenteditable (e.g.
Gmail compose), Word, and an Electron app (VS Code or Slack):

1. **Debug > Focused Field Probe**: click it, switch to the target within 5 s. Shows
   role/subrole/length/selection. Expect role `AXTextArea` (or `AXComboBox`) for editable
   fields; subrole is the UIA framework id ("Win32", "Chrome", "XAML"...).
2. **Live typing**: text appears while speaking; edit mid-dictation, it must continue at the
   cursor; stop, the field must not be rewritten and the clipboard must hold the full text.
3. **Auto-learn**: after a dictation fix a word by hand; within ~60 s a toast offers the pair.
4. **Names around the cursor** (Advanced, `window_context_vocabulary`): a name visible in the
   field comes out spelled right.
5. **Silence stop**: 60 s of silence stops the recording.
6. **Password fields** must never be read (`is_secure` → `CurrentIsPassword`).

Logs: Settings has an "open log directory" button; live typing logs why it did not start
(role, unverified field, copied fallback). Ask the user to paste them.

## Likely problems and where to fix them

- **A field is not recognised as text** → `FocusedField::role()` in `accessibility_uia.rs` maps
  UIA control types to AX role names so `is_text_target` / `is_building_role` in
  `live_typing.rs` stay platform-neutral. Edit/ComboBox map directly; anything else is
  "AXTextArea" only if it has TextPattern and is not read-only; read-only Document is
  "AXWebArea" (treated as still loading). Adjust the mapping, not `live_typing.rs`.
- **Chromium/Electron expose nothing** → `enable_manual_accessibility` is a no-op on Windows,
  assuming Chromium builds its UIA tree on the first UIA query. If not, Chromium can be forced
  with `--force-renderer-accessibility`, or by sending `WM_GETOBJECT` to the window.
- **Wrong text around the cursor / bad deletes** → UIA has no UTF-16 offsets. Text is read by
  moving `TextUnit_Character` from the caret and trimmed to exact UTF-16 counts
  (`text_before_cursor`, `text_after_cursor`). `selected_range().location` is capped at 10_000
  units and only `length` is relied on. Watch CRLF in Notepad/Word (may count as one unit).
- **Hangs** → UIA timeouts: 200 ms connection / 500 ms transaction via `IUIAutomation2`.
- **Elevated apps** (run as admin) can't be read or typed into unless Handy is elevated too.
- **Threading** → UIA calls must stay off the Tauri main thread (they run on the live typer,
  correction watcher and window-context threads; never inside `on_main` closures). COM is
  joined as MTA per thread in `automation()`.

## Open items not tied to Windows

- **"Scratch that" barely works, especially in Italian** (user report). Measure with the
  Italian/English self-correction eval in `scripts/dictation-eval/` (`selfcorr.py`,
  `selfcorr.jsonl`) before changing the rule in `src-tauri/src/audio_toolkit/text.rs`.
  Likely cause: `SCRATCH_THAT_PATTERN` there only matches the English "scratch that" /
  "delete that"; there is no Italian trigger phrase at all.
- Only `en` and `it` locales say "macOS and Windows only"; the other 23 still say "macOS only"
  for live typing, auto-learn and window context.
- The silence-stop time is a constant; a setting and a toast were deliberately left out.

# Offhand

Push-to-talk dictation for Windows that runs entirely on your PC. Hold a
hotkey, speak, let go, and the text is pasted wherever your cursor is. No
account, no subscription, and no audio leaves the machine. An optional
Smart mode sends the text, never audio, to Claude Haiku for fixes that
depend on meaning.

Status: early (v0.6). Works on Windows 10/11 x64. English plus 24 European
languages (whatever Parakeet v3 supports).

## Why it's light

A dictation app runs all day, so idle cost matters more than anything else.

- **Idle:** no mic open, no model in memory, no browser engine. Measured on a
  Ryzen 9 9900X with the floating pill on screen: about 22 MB working set,
  5 MB private memory, 9 threads, and no measurable CPU.
- **The floating pill is not a webview.** It's a layered window drawn with
  Direct2D in software, so no GPU driver is loaded into the process and the
  GPU stays free for games. It repaints when its state changes and at a
  10-second check-up that keeps it on screen, and animates only while you
  talk.
- **The mic opens only while the hotkey is held.** No privacy light, no
  Bluetooth headset stuck in call mode.
- **The model loads while you speak** (about 1.8 s, hidden behind your
  speech). It unloads after 15 minutes idle, which is configurable. About
  800 MB while loaded.
- **Speed:** 12 s of speech transcribes in about 750 ms on CPU, with no GPU
  needed.
- **The app window is opened on demand and destroyed on close.** Nothing
  web-based runs while Offhand waits in the tray.

## Using it

Hold **Ctrl + Win** (or the shortcut you pick), talk, release.

**Hands-free mode** (Settings, or the tray and pill menus):
- Tap the shortcut to start, and tap it again (or click the pill) to finish
  and paste.
- Holding still works for a quick burst. Esc cancels.
- The pill shows a stop square while it's listening hands-free.

The floating pill shows what's happening. Idle, it's a thin underline. While
you talk, a pill lifts above it and the underline turns blue. The underline
pulses while the text is written, then turns red if something's wrong. Drag
the pill anywhere and it stays there. Right-click it for quick actions;
double-click to open Offhand. By default it's always on screen; Settings can
keep it out of fullscreen apps, or show it only while you're talking.

The app window has four pages:
- **Home:** weekly words, pace and time saved, plus your latest dictations.
- **History:** every dictation, grouped by day, with search. Copy one, copy
  what you said before cleanup, or delete it.
- **Dictionary:** names and terms Offhand should spell your way, with what
  speech recognition sometimes writes instead.
  - When you fix a name right after it's pasted ("Neve" to "Niamh"),
    Offhand learns it and the pill says so.
  - It also learns a number inside a name ("Nifty fifty Solutions" retyped
    as "Nifty 50 Solutions") and a name's capitals ("github" to "GitHub").
  - It only learns names and terms that sound like what was heard, so
    grammar edits, emphasis and changed meanings are ignored.
- **Settings:**
  - shortcut (Ctrl+Win, Right Ctrl, Right Alt, Ctrl+Alt, Ctrl+Shift)
  - when to show the pill
  - microphone and level test
  - mute other apps while dictating (stops YouTube or music leaking into the
    mic)
  - sounds, start with Windows, trailing space
  - how long to keep the model loaded
  - text cleanup (Off, Light or Smart, plus the API key for Smart)
  - learning from corrections
  - history (on this PC only; keep 7 days, 30 days, a year or forever)

Pressing any other key while the chord is held cancels, so Ctrl+Win+D and
similar shortcuts still work.

## Text cleanup

**Light** cleanup is on by default. It's deterministic rules that take
microseconds:
- drops um, uh, ah, ooh, hmm and stretched versions ("uhhh"), and fixes the
  commas around them; meaningful words like "oh", "mhm" and "ER" stay
- collapses stutters ("I I think") and repeated phrases ("you know what,
  you know what")
- writes spoken shortcuts as keys: "control plus C" becomes Ctrl+C, "alt
  tab" becomes Alt+Tab ("shift a meeting" and "control panel" stay as words)
- understands "bullet point" (list items) and "quote … end quote"
- turns a spoken "new line" or "new paragraph" into a break
- applies your Dictionary
- terminals get bare commands ("git status"), and one-line chat messages
  lose the final full stop

It never rewords, and it makes no false edits on the seed cases. The cases
it can't pass need context. With Smart, 45 of the 46 cases pass.

**Smart** is opt-in. It's Light plus a pass by Claude Haiku 5.5
(`src-tauri/src/smart.rs`) for what depends on meaning:
- numbers: "fifty signups" becomes 50 signups, "twenty dollars" becomes
  $20, "three thirty pm" becomes 3:30 PM, but "one of them" and "two
  options" stay words
- self-corrections: "Tuesday, no wait, Wednesday" becomes "Wednesday"
- counted lists: "first …, second …, third …" becomes a numbered list
- typed forms: "sam at example dot com" becomes sam@example.com,
  "package dot json" becomes package.json

How it behaves:
- **Only some text is sent.** It goes out only when it has one of those
  cues (a number word, "no wait" or "sorry", "first", "dot"). Everything
  else pastes at Light speed and never leaves the PC.
- **Audio never leaves the PC.** Only the text is sent, along with the
  app's name and your Dictionary words as spelling hints.
- **It never holds you up.** A call takes about 1 second (p90 under
  2 s on the test cases). If a reply takes more than 3 seconds, or looks
  like a rewrite or an answer, the Light text is pasted.
- **It runs on your own Anthropic API key.** The key is kept in Windows
  Credential Manager. At Haiku's price ($0.10 / $0.50 per million tokens), a
  call costs about $0.0001.

See `docs/AI-CLEANUP-PLAN.md`.

## How it works

| Piece | File | Notes |
| --- | --- | --- |
| Hotkey | `src-tauri/src/hotkey.rs` | Low-level keyboard hook, so modifier-only chords and key-up work. Suppresses the Start menu after Win chords. |
| Mic | `src-tauri/src/audio.rs` | cpal/WASAPI, opened on press, closed on release. Windowed-sinc resample to 16 kHz. Splits long audio at quiet points. Feeds peak levels to the pill. |
| Speech | `src-tauri/src/engine.rs` | NVIDIA Parakeet TDT 0.6B v3 (int8 ONNX) via `transcribe-rs`, on CPU. |
| Cleanup | `src-tauri/src/cleanup.rs` | The Light rules above, plus the `--bench-cleanup` harness. |
| Smart | `src-tauri/src/smart.rs` | Claude Haiku 5.5 over plain HTTPS, with the cue gate, a 3 s timeout and reply checks. The key is in Windows Credential Manager (`secret.rs`). It connects only while you dictate, opening the connection as you start talking. |
| Pill | `src-tauri/src/pill.rs` | WS_EX_NOACTIVATE layered window, Direct2D (software) + DirectWrite, its own thread. |
| Learning | `src-tauri/src/learn.rs` | From the first paste into a box until the message is sent (the pastes leave the box), or 2 minutes after the last paste, UI Automation reads that one field (never password fields): at each Enter, before a chat app sends and clears it, and twice a second, keeping the last reading that held still. Every dictation pasted into the box is then aligned with that reading, which is dropped. A fix is learned if it sounds like what was heard and is a name (a capital) or a term the Windows spell checker doesn't know, but not a one-letter typo. The log says why other edits were skipped, without any text. The client exists only for that check. |
| History | `src-tauri/src/transcripts.rs` | One JSON line per dictation, with retention and per-item delete. |
| Paste | `src-tauri/src/clipboard.rs` | Snapshot every clipboard format, paste with Ctrl+V, restore after 400 ms unless you copied something new. Writes are hidden from Win+V history. |
| Mute | `src-tauri/src/mute.rs` | Per-app mute of sessions that are playing; crash-safe (restored on next start). |
| Model | `src-tauri/src/model.rs` | 670 MB, downloaded once from a pinned Hugging Face commit, SHA-256 checked, resumable. |
| App window | `index.html`, `src/` | Plain TypeScript and CSS. Geist is bundled locally. |

Data lives in two places:
- `%APPDATA%\Offhand\`: settings and dictionary.
- Windows Credential Manager: the API key for Smart, if you saved one.
- `%LOCALAPPDATA%\Offhand\`: the model, `offhand.log`, history
  (`transcripts.jsonl`), and weekly stats (counts only).

The log records timings and lengths, never transcript text.

Windows quietly redirects AppData for anything started by a packaged app (one
from the Microsoft Store or an MSIX installer, such as the Claude desktop
app) into that app's private folder. Data saved that way disappears as soon
as Offhand is started normally. Offhand checks for this at startup by writing a
probe file. If its files are being redirected, it restarts itself through
Explorer, outside the package. It also moves back, and merges, anything an
earlier run left in a package's private folder (`src-tauri/src/storage.rs`).

## Known limits

- **Admin windows:** the hotkey doesn't fire while an elevated (admin) window
  has focus. Windows blocks input hooks from normal apps into elevated ones.
  This applies to some games and anti-cheat launchers too.
- **Paste:** apps that ignore Ctrl+V won't receive text. It stays in the
  pill's "Copy last transcript" and on the Home page.
- **Learning** needs the app to expose its text box to Windows UI Automation.
  Most apps do; terminals and games usually don't. In Chromium-based apps,
  reading the field switches on their accessibility support, which costs
  them a little CPU. Turn learning off in Settings if that matters.
- **Language:** no Urdu yet. A Whisper option is planned.
- **Windows only:** macOS and Linux need their own hotkey, paste, mute and
  pill code.

## Build

Needs Rust (MSVC toolchain), Visual Studio Build Tools with the C++ workload,
Node 20+ and pnpm.

```bash
pnpm install
pnpm tauri build --no-bundle     # exe at src-tauri/target/release/offhand.exe (never plain cargo build)
pnpm tauri build                 # NSIS installer
```

To rebuild and restart a running Offhand in one step, run
`scripts/release-local.ps1`. It always builds through Tauri (plain
`cargo build --release` leaves out the app window's pages, so the window
tries `localhost`) and starts the new copy through Explorer.

Checks:
- `cd src-tauri && cargo test` runs the unit tests.
- `scripts/e2e.ps1` runs an end-to-end paste test. It takes keyboard focus
  for about 10 seconds.
- `offhand.exe --bench file.wav` (16 kHz mono 16-bit) prints model load and
  transcription times.
- `offhand.exe --pill-demo`, sent to a running Offhand, walks the pill through its
  states.

Icons come from `scripts/make-icons.ps1`. It needs Geist ExtraBold as a TTF
from the [Geist release](https://github.com/vercel/geist-font/releases).

## Tuning text cleanup

- **Collect:** keep "Keep history on this PC" on in Settings. Each
  dictation is appended to `%LOCALAPPDATA%\Offhand\transcripts.jsonl` with the
  raw and cleaned text, the app's exe name (never window titles), the speech
  length and the timing. It's off by default, nothing is uploaded, and
  "Delete all" removes the file.
- **Measure:** `offhand.exe --bench-cleanup eval/cases.jsonl` runs the
  hand-written cases and prints every failure. Point it at
  `transcripts.jsonl` to see what cleanup changes in real dictations.
- **Smart:** add `--smart` to send the cases that have a cue to the API
  (the saved key, or `ANTHROPIC_API_KEY`). It also prints p50 and p90
  latency.

## Roadmap

1. More Smart options: an OpenAI-compatible endpoint (Groq, OpenRouter, a
   local Ollama on your own GPU), and optional tone.
2. "Only hears you": echo cancellation against system audio plus voice
   enrollment, so other voices and speaker output are ignored even without
   muting.
3. Signed installer and auto-update.

## License

MIT, see [LICENSE](LICENSE). The speech model is a separate download under
its own license (CC BY 4.0).

## Credits

- Speech model: [NVIDIA Parakeet TDT 0.6B v3](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3),
  CC BY 4.0, ONNX export by [istupakov](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx).
- [transcribe-rs](https://github.com/cjpais/transcribe-rs) (MIT) for Parakeet inference.
- [Geist](https://github.com/vercel/geist-font) (OFL) via Fontsource.
- Built with [Tauri](https://tauri.app).

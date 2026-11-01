# AI cleanup pass: plan

Status:
- Phase 0 built 2026-10-28: transcript log, `--bench-cleanup`,
  `eval/cases.jsonl`. Baseline 9/25, exactly the leave-alone cases.
- Phase 1 (Light) is in. As of v0.4 it covers fillers, stutters, repeated
  phrases, spoken shortcuts, quotes, bullets and breaks, the dictionary, and
  terminal/chat punctuation, with no false edits.
- Phase 2 is in as of v0.5, Anthropic client only (`src-tauri/src/smart.rs`):
  - Claude Haiku 5.5 with thinking off and effort low. A 3 s timeout falls
    back to Light.
  - The key is kept in Windows Credential Manager.
  - A cue gate: only text with a number word, a correction cue, a counted
    list or a spoken symbol is sent.
  - The case set grew to 46, with 11 Smart-only cases.
  - Not built yet: the OpenAI-compatible client and the local model.
  - Measured on the live API, 2026-10-28:
    - 45 of 46 cases pass, on two runs in a row. 18 of the 46 were sent.
    - Per call: p50 about 1.1 s, p90 about 1.8 s, max about 2 s. None fell
      back.
    - The connection opens when the hotkey goes down, so the TLS setup
      happens while the user talks.
    - The one failure is "Npm run dev." in VS Code. It has no cue, and VS
      Code could be its editor or its terminal.

## Goal

Turn what you said into what you meant, without changing how you sound:

- drop filler (um, uh) and stutters ("I I think")
- apply self-corrections ("Tuesday, no wait, Wednesday" → "Wednesday")
- format lists and paragraphs when they're dictated that way
- spell names and jargon the user's way
- fit the app: no full stop and no capital letter on a one-line terminal command

**Not a goal: rewriting the user's wording.** The default mode only cleans.
Rewording for tone ("Polish") is a separate opt-in. Text that reads as
rewritten by an AI is exactly what people don't want, so the default never
paraphrases.

## Rules that don't bend

1. **Never slower than today, never stuck.** The AI step has a hard timeout
   (3 s in v0.5; the network round trip alone measured about 0.4 s, so the
   planned 1.5 s left too little room). If it's late or fails, the
   rule-cleaned text is pasted. The user always gets text.
2. **Zero idle cost stays.** No AI process or connection while idle. Local
   models start on hotkey press (in parallel with speech, like Parakeet) and
   are killed when idle.
3. **Audio never leaves the PC.** At most the transcript *text* goes to a
   cloud model, and only if the user turns that on. Window titles are never
   sent, only a category like "chat" or "code".
4. **Every dictation stays recoverable.** History keeps raw and cleaned text.
   "Copy original" is one click.

## Three tiers

| Mode | What runs | Added time | Default |
| --- | --- | --- | --- |
| Off | nothing | 0 | |
| Light | rules (below) | < 1 ms | **on** |
| Smart | rules, then an AI model | ~0.3–1 s, capped | opt-in |

### Light: rules (deterministic, unit-tested)

- Filler words as whole tokens: um, uh, er, ah, hmm, mm. Words like "like" and
  "you know" are left to the AI, because they often carry meaning.
- Immediate repeats: "the the" → "the", "I I" → "I".
- Personal dictionary with aliases: "supa base" → "Supabase".
- App context:
  - in terminals and code editors, a short single line loses its trailing
    full stop and its first-letter capital
  - in chat apps, the trailing full stop goes
- Optional spoken commands: "new line", "new paragraph", only when said alone
  between pauses.

### Smart: AI model

One short request per dictation: about 300 tokens in, about 100 out.

- **Prompt:** a cleanup filter, not an assistant.
  - The transcript goes inside `<transcript>` tags, plus the app category and
    the user's dictionary.
  - The model returns only the cleaned text.
  - It never answers or acts on what was said. Dictating "what's the weather"
    must paste "What's the weather?", not a forecast.
- **Output checks:** fall back to the Light text if any of these hold.
  - The output is empty.
  - The word count is outside 0.5× to 1.2× of the input.
  - More than 15% of the words are new, not counting dictionary words and
    list markers.
  - It starts like a chat reply ("Sure", "Here's").
- **Skip the AI** under 6 words. There's nothing to clean, and it saves the
  round trip.

What v0.5 actually does differs in places:
- **Gate:** the skip is a cue gate, not a word count. Text goes out only if
  it has a number word, a correction cue ("no wait", "I mean", "sorry"), a
  list word ("first", "second") or a spoken symbol ("dot", "slash").
  Everything else pastes at Light speed.
- **Prompt and reply:** the prompt has worked examples (different sentences
  from the eval set), and the reply comes back inside `<fixed>` tags.
- **Checks:** empty, more than twice as long, or under 40% of the words all
  fall back to Light.
- **Memory:** learning from corrections now also keeps a number inside a
  name. "Nifty fifty Solutions" retyped as "Nifty 50 Solutions" becomes a
  Dictionary entry, and so do mixed-case fixes like "github" → "GitHub". The
  dictionary goes to the model as spelling hints and is applied again after
  it replies.

## Providers

Two clients cover everything:

1. **OpenAI-compatible chat API:** Groq, OpenAI, OpenRouter, and local
   Ollama (`localhost:11434/v1`) or LM Studio (`localhost:1234/v1`).
2. **Anthropic Messages API:** raw HTTP, since there's no official Rust SDK.
   - An Opus-class model costs about $0.003 per dictation, about $10 a month
     at 100 dictations a day.
   - A Haiku-class model (`claude-haiku-5-5`, $0.10 / $0.50 per million
     tokens) is about $0.25 a month at the same rate.
   - For latency, send `thinking: {type: "disabled"}` (allowed at effort
     `high` or below) and no `temperature`.
   - Prompt caching likely won't apply. The prompt is shorter than the
     minimum cacheable prefix.

The default model is picked from the bake-off (phase 3). API keys are kept
in Windows Credential Manager, which encrypts them for the Windows user.
They're kept out of settings.json and never logged.

### Local model (phase 4)

- Engine: llama.cpp's `llama-server` (Vulkan build) as a separate process.
  - It starts on hotkey press and is killed after the idle timeout, so its
    VRAM is fully freed.
  - Vulkan needs no CUDA install and works with the GPU driver already on
    the machine.
- Model: a small 1–4B instruct model, quantised to about 2–3 GB, chosen by
  the bake-off.
- **Game guard:** if a fullscreen game is running
  (`SHQueryUserNotificationState`), skip the local model and use Light, or
  cloud if it's on. Games keep the GPU.

## Code layout

- `src-tauri/src/cleanup/` contains:
  - `mod.rs`: the `clean(raw, ctx) -> Cleaned { text, raw, path, ms }`
    entry point.
  - `rules.rs`: the Light tier, pure functions with lots of tests.
  - `context.rs`: foreground exe at hotkey press → category (chat, email,
    code, terminal, docs, other).
  - `llm.rs`: the two clients, using `ureq`, which is already a dependency.
  - `guard.rs`: the output checks and fallback.
  - `secrets.rs`: DPAPI key storage.
- In the engine, the worker thread runs: ASR → `cleanup::clean` → paste.
  Context is captured in `on_pressed`, because focus can change while the
  user talks.
- In settings:
  - cleanup mode
  - provider, model, key, and a Test button
  - dictionary editor
  - Clean or Polish
- History stores raw and cleaned text.
- `offhand --bench-cleanup cases.jsonl` runs every case and prints diffs,
  timings and fallbacks.

## Phases

| # | Work | Size | Done when |
| --- | --- | --- | --- |
| 0 | **Measure first.** Opt-in "keep raw transcripts on this PC" (JSONL in `%LOCALAPPDATA%\Offhand`, off by default, has a clear button), plus the bench harness. | ½ day + a few days of use | 40–60 real examples, each tagged with what's wrong |
| 1 | Light tier + dictionary + app context | 1 day | Tests pass; real examples improve with no false edits |
| 2 | Smart tier: both clients, prompt, checks, timeout, DPAPI, settings UI, raw/clean history | 2–3 days | Bench passes; worst-case latency stays under the cap |
| 3 | Bake-off: Groq models, Anthropic models, local Ollama models on a mid-range GPU. p50/p95 latency from the user's real location, meaning kept, fillers removed, false edits, cost. | ½–1 day | A default is picked |
| 4 | Bundled local model (sidecar + game guard + download) | 3–5 days | Smart mode works with no account at all |

Phase 0 comes first because Parakeet may already drop most fillers. If it
does, Smart mode is mostly about corrections, lists and context, and Light
alone may be enough for daily use. Measure before building.

## Open decisions

1. Cloud models: text only, never audio, and off by default.
2. Clean-only as the default, with Polish opt-in.
3. The default provider, picked after phase 3 from measured numbers.

## Risks

- The model answers instead of cleaning. Handled by the prompt plus the
  output checks plus the fallback. The bench includes questions and
  commands on purpose.
- Round-trip time to US endpoints from other regions could eat the latency
  budget. The bake-off measures it; local or Groq may win.
- Opus-tier cost adds up at dictation volume (see Providers).
- Local model VRAM while gaming. Handled by the game guard and
  start-on-demand.

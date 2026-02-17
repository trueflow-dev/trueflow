# TUI Speed Reading Mode (Prose Review)

## Goal
Add an optional TUI mode for reviewing prose quickly, inspired by RSVP readers (e.g. AccelaReader), while preserving review actions (`approve`, `comment`, `reject`) and traceability.

## Non-Goals
- Do not replace normal code-review mode.
- Do not optimize for code syntax; this mode targets prose-heavy blocks (`Paragraph`, `TextBlock`, docs).
- Do not require network services or external dependencies.

## Primary User Story
As a reviewer reading long prose blocks, I want words/phrases shown in a stable center position at controlled WPM so I can stay focused, pause instantly, and annotate/approve without losing context.

## UX Summary
- Enter mode from TUI when current node is prose-capable.
- Screen shows:
  - Active phrase at visual center (high contrast).
  - Minimal context strip (previous/next phrases dimmed).
  - Status row: WPM, chunk size, progress %, elapsed/remaining.
  - Existing action hints along bottom.
- Supports manual stepping and autoplay.

## Keybindings (Proposed)
- `r`: toggle speed-reading mode (from normal prose view).
- `Space`: play/pause.
- `h` / `l`: previous/next phrase.
- `-` / `=`: decrease/increase WPM (coarse).
- `[` / `]`: decrease/increase chunk size (words per phrase).
- `0`: reset speed/chunk to defaults.
- `Esc`: exit speed-reading mode back to normal block rendering.
- Keep existing review actions active (`a`, `c`, `x`, navigation).

## Data Model
Add a pure core module (e.g. `review_speedread.rs`) with:

- `SpeedReadState`
  - `tokens: Vec<String>`
  - `phrases: Vec<Phrase>` (pre-chunked)
  - `cursor: usize`
  - `playing: bool`
  - `wpm: u16`
  - `chunk_words: u8`
  - `focus_char_index: Option<usize>` (for optional ORP highlight)
- `Phrase`
  - `text: String`
  - `start_token: usize`
  - `end_token: usize`

Core functions:
- `tokenize_prose(text) -> Vec<String>`
- `build_phrases(tokens, chunk_words) -> Vec<Phrase>`
- `step_next/step_prev`
- `autoplay_tick_interval_ms(wpm, chunk_words) -> u64`
- `rechunk_preserving_progress(state, new_chunk_words) -> SpeedReadState`

Keep this logic out of TUI rendering code.

## Rendering Model
- TUI only consumes `SpeedReadState` and renders:
  - Center line: active phrase in bold.
  - Above/below context lines in dim style.
  - Bottom meter: progress and settings.
- Optional ORP-style highlight:
  - Highlight one anchor character in phrase for easier fixation.
  - Controlled via config flag.

## Input/Timing
- Reuse existing event loop with timeout-based polling while `playing`.
- On each timer tick:
  - advance phrase cursor,
  - stop at end unless loop mode enabled.
- Any user keypress immediately interrupts autoplay latency.

## Mode Eligibility
Enable when current node maps to prose-like kinds:
- `Paragraph`, `TextBlock`, `Comment`, Markdown-derived prose blocks.
Fallback:
- if block is code-heavy, show "Speed mode optimized for prose" and allow force-enable via config.

## Config (Proposed)
`[tui.speed_read]`
- `enabled = true`
- `default_wpm = 320`
- `default_chunk_words = 2`
- `min_wpm = 120`
- `max_wpm = 900`
- `show_orp_highlight = false`
- `loop_playback = false`

## Testing Strategy
1. Core unit tests (required)
- tokenization edge cases (punctuation, unicode, whitespace)
- phrase chunking correctness
- cursor bounds and step behavior
- rechunk keeps semantic progress
- tick interval math clamps

2. TUI unit tests
- mode toggles only on eligible blocks
- key handlers mutate state correctly
- pause/play transitions are deterministic

3. Integration tests
- prose fixture: enter mode, autoplay N ticks, exit, submit review action.

## Performance Notes
- Precompute phrases once per block entry.
- Avoid reallocating phrases on each tick.
- Rechunk only when chunk size changes.
- Keep render payload tiny (3–5 lines) for low flicker.

## Rollout Plan
1. Phase 1: manual stepping only (`playing=false` only).
2. Phase 2: autoplay timer and WPM controls.
3. Phase 3: optional ORP highlight and config surface.
4. Phase 4: heuristics for sentence-aware pauses (comma/period weighting).

## Open Questions
- Should sentence punctuation impose automatic dwell multipliers?
- Should comment entry auto-pause and preserve phrase cursor?
- Should this mode be available from root/file nodes (multi-block prose streams), or block-only initially?
- Do we persist per-user last WPM/chunk settings between sessions?

# TUI Speed Reading Mode (Prose Review)

## Status
- Existing roadmap item: `TODO.org` has a speed-reading TODO under Product and Platform.
- Existing draft: this document started as a high-level proposal.
- This revision defines an implementation-grade plan aligned to the current TUI architecture in `trueflow/src/commands/tui.rs`.

## Problem Statement
Reviewing prose blocks (`Paragraph`, `TextBlock`, markdown-heavy content) in the current TUI still uses the same source/diff reading flow as code. That is slower and has more fixation drift than an RSVP-style center focus mode.

We want a focused reading mode where each word or phrase appears in a stable center position, with a center "sight bar" tick under the fixation point, configurable words-per-minute, and no loss of existing review actions (`approve`, `comment`, `reject`).

## Goals
- Add an optional prose-focused speed-reading mode inside the existing TUI.
- Keep review actions and review traceability identical to current behavior.
- Keep all timing/chunking logic in a pure core module (testable without terminal rendering).
- Preserve current TUI mode as default and fallback.
- Persist speed-reading user settings in `trueflow.toml`.

## Non-Goals
- No replacement of normal source/diff mode.
- No dependency on external services or non-Rust runtime tooling.
- No code-syntax optimization for this mode in v1.
- No multi-block stream mode in v1 (block-only first).

## User Story
As a reviewer reading prose blocks, I want the current phrase to stay centered with a visible fixation tick, so I can read faster with less eye movement, pause immediately, step backward/forward, and still act on the block without context loss.

## Current TUI Constraints
- Event loop currently blocks on `event::read()`.
- Existing view state is `ViewMode::{Diff, Source}`.
- Key handling is centralized in `run_app` with `InputMode::{Normal, Editing, ConfirmBatch}` overlays.
- Content rendering is centralized in `render_active_node -> build_content_lines`.
- Config currently exposes `[tui]` fields only (`confirm_batch`, `diff_focus_mode`, `diff_focus_context_lines`).

## Proposed UX
When enabled and active on an eligible block:
- Content pane shows:
  - Previous phrase (dim, centered).
  - Active phrase (bold/high contrast, centered).
  - A one-character center sight bar tick under the active phrase (default `^`).
  - Next phrase (dim, centered).
  - Status line: `WPM`, `chunk`, `phrase_index/phrase_total`, and progress percent.
- Footer/actions remain visible and include speed-read hints.
- Review actions (`a`, `c`, `x`) remain active via the existing action paths.

## Keybindings
Global:
- `r`: toggle speed-reading mode for current block if eligible.
- `Esc`: exit speed-reading mode back to normal block rendering.

Inside speed-reading mode:
- `Space`: play/pause autoplay.
- `j`: previous phrase.
- `l`: next phrase.
- `-`: decrease WPM.
- `=`: increase WPM.
- `[`: decrease chunk size (words per phrase).
- `]`: increase chunk size.
- `0`: reset WPM/chunk to configured defaults.

Notes:
- `j/l` override normal nav only while speed-read mode is active.
- Normal navigation remains unchanged when speed-read mode is not active.
- `Space` is always play/pause while speed-read mode is active.

## Eligibility Rules (v1)
Permissive by default:
- Any `TreeNodeKind::Block` can enter speed-reading mode.
- If text appears code-heavy, show a hint label only:
  - `Speed mode is prose-optimized; code accuracy may be better in source/diff mode.`
- No hard blocking by block kind.

## Architecture
Add new pure module: `trueflow/src/review_speedread.rs`

Core types:

```rust
pub struct SpeedReadModel {
    pub tokens: Vec<Token>,
    pub phrases: Vec<Phrase>,
    pub cursor: usize,
    pub playback: PlaybackState,
    pub settings: SpeedReadSettings,
}

pub struct Token {
    pub text: String,
    pub is_word: bool,
}

pub struct Phrase {
    pub text: String,
    pub start_word_index: usize,
    pub end_word_index: usize, // exclusive
    pub anchor_char_index: usize,
}

pub enum PlaybackState {
    Paused,
    Playing,
}

pub struct SpeedReadSettings {
    pub wpm: u16,
    pub chunk_words: u8,
}
```

Core functions:
- `tokenize_prose(text: &str) -> Vec<Token>`
- `build_phrases(tokens: &[Token], chunk_words: u8) -> Vec<Phrase>`
- `step_next(model: &mut SpeedReadModel, loop_playback: bool)`
- `step_prev(model: &mut SpeedReadModel)`
- `set_wpm(model: &mut SpeedReadModel, new_wpm: u16, min: u16, max: u16)`
- `set_chunk_words(model: &mut SpeedReadModel, new_chunk: u8, min: u8, max: u8)`
- `rechunk_preserving_progress(model: &mut SpeedReadModel, new_chunk_words: u8)`
- `tick_interval_ms(wpm: u16, chunk_words: u8) -> u64`
- `next_wpm_step_up(current_wpm: u16) -> u16`
- `next_wpm_step_down(current_wpm: u16) -> u16`

Timing formula:
- `tick_ms = (60_000 * chunk_words as u64) / wpm as u64`
- Clamp to a safe range (for example `30..=2000`) to avoid runaway behavior.

WPM stepping (multiplicative/geometric):
- Use ratio `r = 2^(1/6) ~= 1.122462`.
- `increase`: `wpm = round(wpm * r)`.
- `decrease`: `wpm = round(wpm / r)`.
- Properties:
  - 6 steps is exactly 2x speed.
  - Steps are scale-invariant (same relative change at any speed).
  - Large enough to be noticeable, small enough for quick tuning.

Chunk stepping:
- `[`/`]` change `chunk_words` by exactly `1` per press.

Punctuation dwell (release default):
- Mode: `light`.
- If a phrase ends with `, ; : . ! ?`, multiply dwell time by `1.15`.
- Otherwise use base `tick_interval_ms`.

## TUI State Integration
Add to `AppState`:
- `speed_read: Option<SpeedReadUiState>`

```rust
struct SpeedReadUiState {
    node_id: TreeNodeId,
    model: SpeedReadModel,
    last_tick: std::time::Instant,
    eligibility: EligibilityState,
}

enum EligibilityState {
    Eligible,
    Ineligible { reason: String },
}
```

Rules:
- The state is scoped to the currently selected block hash/node.
- On node change, speed-read state is dropped unless new node matches and reuse is explicitly desired (v1: drop/reset).
- On reaching final phrase in autoplay:
  - set playback to paused,
  - exit speed-read mode,
  - return to normal full-block rendering view.

## Event Loop Changes
In `run_app`:
- Replace unconditional `event::read()` with:
  - `event::poll(timeout)` then `event::read()` when an event exists.
- Timeout policy:
  - Not playing: long poll (or blocking behavior equivalent).
  - Playing: timeout = remaining time to next tick.
- On timeout while playing:
  - advance phrase cursor,
  - stop at end unless `loop_playback` is enabled.

This keeps keypress latency low while autoplay is active.

## Rendering Changes
In `render_active_node`:
- If `speed_read` is active and targeted at current node:
  - bypass normal `build_content_lines` for content pane,
  - render speed-read frame lines (prev/current/tick/next/status).
- Keep header and footer rendering unchanged.

Center/tick behavior:
- Active phrase centered to content width.
- Tick rendered on next line at exact center column.
- Optional anchor highlight (`show_orp_highlight`) can color one char in active phrase.

## Config Additions
Extend `TuiConfig` in `trueflow/src/config.rs`:

```toml
[tui.speed_read]
enabled = true
default_wpm = 320
min_wpm = 120
max_wpm = 900
default_chunk_words = 2
min_chunk_words = 1
max_chunk_words = 5
loop_playback = false
show_orp_highlight = false
show_prose_optimization_hint = true
punctuation_dwell = "light" # off|light
punctuation_dwell_multiplier = 1.15
```

Implementation note:
- Use strongly typed structs/enums in config parsing.
- Keep defaults conservative.

## Settings Persistence
- Persist mutable speed-reading settings to `trueflow.toml`.
- Persisted fields:
  - `tui.speed_read.default_wpm`
  - `tui.speed_read.default_chunk_words`
- Use `toml_edit` to preserve existing comments and formatting while updating only targeted keys.
- Write policy:
  - Debounce writes during active key changes (`512ms`).
  - Always flush final value at end of session.
  - On `q` quit, force immediate final flush even if debounce has not elapsed.
  - If `trueflow.toml` does not exist, create it at the repository root on first persisted write.
  - `0` resets to persisted defaults loaded from `trueflow.toml`.

## Testing Strategy (TDD)
1. Core tests first (`src/review_speedread.rs`):
- Tokenization with punctuation, unicode, multi-space, and newline inputs.
- Phrase building with exact and tail chunk sizes.
- Cursor stepping at boundaries with/without loop.
- Rechunk preserving semantic progress.
- Tick interval calculations and clamping.

2. TUI unit tests (`src/commands/tui.rs`):
- Toggle only activates on eligible block kinds.
- Key handling in speed-read mode mutates state as expected.
- `a/c/x` actions run through existing execute paths while speed mode is active.
- Mode exits on `Esc` and on non-block navigation.

3. Integration tests:
- Add prose-heavy fixture case (likely markdown) and test mode-eligibility helpers.
- Keep terminal rendering assertions unit-level; avoid brittle full TTY interaction in e2e.

## Performance and Safety
- Precompute phrases once when entering mode.
- No per-tick reallocations except cursor/index updates.
- Cache invalidation on node change is explicit.
- Keep overlay behavior unchanged from current TUI semantics.

## Rollout Plan
Phase 1:
- Core model + manual stepping only (no autoplay yet).
- Toggle, render, chunk/WPM controls wired but playback remains paused.

Phase 2:
- Event-loop timeout integration and autoplay.
- Pause semantics for overlays/actions.

Phase 3:
- Config surface and ORP anchor highlight.
- Eligibility heuristic plus ineligible reason messaging.

Phase 4:
- Optional punctuation dwell multipliers and sentence-aware pacing.
- Evaluate multi-block stream mode as separate design.

## Acceptance Criteria
- Reviewer can enter speed mode on any block (`TreeNodeKind::Block`) with `r`.
- Active phrase remains centered and tick is visible below center.
- `Space` toggles autoplay deterministically.
- `j/l` step works at boundaries without panic.
- `a/c/x` still work from speed mode using existing action flows.
- Defaults work with no config file present.
- `just check` stays green.

## Risks and Mitigations
- Keybinding conflict with navigation:
  - Mitigation: mode-local binding table and explicit status hints.
- Event-loop complexity:
  - Mitigation: keep timeout logic isolated and covered by pure helper tests.
- Code-heavy content can be harder to consume in speed mode:
  - Mitigation: permissive mode plus prose-optimization hint in status line.

## Open Questions
- None. Design decisions are locked for implementation.

# Crossterm Best Practices Plan

## Scope

This plan covers how trueflow uses `crossterm` today in the TUI and where that usage should be tightened up or expanded based on `crossterm`'s documented event and terminal-management model.

The goal is not to add every optional terminal feature. The goal is to:

- remove real misuses or fragile patterns
- align the TUI with `crossterm`'s intended lifecycle
- improve text-entry reliability
- make behavior more consistent across terminals

## Summary assessment

The current `crossterm` usage is mostly concentrated in one place, which is good:

- terminal setup and teardown are centralized in `trueflow/src/commands/tui.rs`
- input handling is centralized in the TUI event loop
- `read` and `poll` are used correctly on one thread

The main gaps are:

1. the modal note/comment editor does not support bracketed paste
2. the TUI requests richer key-event types but discards repeat events
3. keyboard enhancement is pushed unconditionally rather than being capability-aware
4. terminal lifecycle is not guarded against partial setup failures or panics
5. the TUI does not fail early when launched without a TTY
6. resize bursts are handled one event at a time rather than coalesced

## Main goals

1. make note/comment editing robust for pasted multiline text
2. make keyboard-event handling consistent with the requested `crossterm` flags
3. reduce terminal-specific ambiguity around progressive keyboard enhancement
4. make terminal setup, suspend, and restore harder to leave in a broken state
5. improve failure mode quality for non-interactive environments
6. reduce unnecessary rerender churn during resize bursts

## Recommended sequencing

### Phase 1: text-entry correctness

Land bracketed paste support first. This is the clearest user-facing improvement and the most obviously missing `crossterm` feature for a multiline text input.

### Phase 2: keyboard-event consistency

Decide whether trueflow wants repeat-key behavior. Right now the code asks for richer key kinds and then ignores part of the result.

### Phase 3: terminal capability and lifecycle hardening

Add capability checks, TTY preflight, and a guard-based terminal session abstraction.

### Phase 4: polish

Coalesce resize storms and optionally add focus change support if it helps speed-read or deferred config flushing.

## Phase 1: bracketed paste support

### Problem

The TUI has a multiline editor for notes/comments, but it never enables bracketed paste and never handles `Event::Paste`.

This means terminal paste behavior is left to legacy key-stream handling, which is a poor fit for multiline modal text entry.

### Desired outcome

- pasted text in the note/comment editor is inserted verbatim
- multiline pastes preserve newlines
- paste does not accidentally act like a sequence of submits or control keys

### Implementation tasks

1. Enable bracketed paste when entering TUI mode.
2. Disable bracketed paste when leaving TUI mode.
3. Handle `Event::Paste(String)` in the main app loop.
4. Restrict paste insertion to `InputMode::Editing`.
5. Append pasted text directly to `input_buffer`.
6. Add tests for:
   - single-line paste into the editor
   - multiline paste into the editor
   - paste ignored outside editing mode
   - terminal setup/teardown includes bracketed paste commands

### Files likely touched

- `trueflow/src/commands/tui.rs`

## Phase 2: key-event consistency

### Problem

The TUI currently requests:

- `DISAMBIGUATE_ESCAPE_CODES`
- `REPORT_ALL_KEYS_AS_ESCAPE_CODES`
- `REPORT_EVENT_TYPES`

But the event reducer only accepts `KeyEventKind::Press`.

That creates a mismatch between requested protocol richness and actual handling.

### Decision required

Pick one of these and be explicit:

1. Support repeat events.
2. Stop requesting `REPORT_EVENT_TYPES`.

### Recommendation

Support repeat events for navigation and scrolling, but not for actions that should stay single-fire.

Concretely:

- allow `Repeat` for movement and scrolling
- keep `Approve`, `Note`, and other destructive or mode-changing actions press-only

### Implementation tasks

1. Split "accept any actionable key event" from "accept navigation/editing key events".
2. Route `Repeat` through movement, page scrolling, and editor text insertion where appropriate.
3. Keep `Release` ignored.
4. Add tests for:
   - held navigation keys repeat
   - held approval/note keys do not auto-repeat into multiple actions

### Files likely touched

- `trueflow/src/commands/tui.rs`

## Phase 3: capability and lifecycle hardening

### 3A: keyboard enhancement capability check

#### Problem

The TUI pushes keyboard enhancement flags unconditionally even though `crossterm` documents limited terminal support and provides `supports_keyboard_enhancement()`.

#### Recommendation

Detect support once during TUI startup, before the event loop begins, and only push/pop enhancement flags when supported.

#### Implementation tasks

1. Add a small `TerminalCapabilities` struct.
2. Probe `supports_keyboard_enhancement()` before entering the main event loop.
3. Thread the capability through terminal setup/teardown.
4. Keep the failure mode conservative:
   - if probing errors, log/debug-note it and continue without enhancement
5. Add tests for:
   - enhancement enabled when supported
   - enhancement skipped when unsupported

### 3B: TTY preflight

#### Problem

The TUI goes straight into raw mode without checking whether stdin/stdout are actual terminals.

#### Recommendation

Fail early with a clear error if stdin or stdout is not a TTY.

#### Implementation tasks

1. Use `crossterm::tty::IsTty`.
2. Check stdin and stdout before raw mode setup.
3. Return a human-readable error.
4. Add tests for the preflight helper.

### 3C: guard-based terminal session

#### Problem

Terminal state transitions are currently hand-managed. Partial setup failure or panic can leave raw mode, alternate screen, mouse capture, or enhancement state inconsistent.

#### Recommendation

Introduce a small RAII-style terminal session guard that owns:

- raw mode state
- alternate screen state
- mouse capture state
- optional bracketed paste state
- optional keyboard enhancement state

This should also be reusable by `with_terminal_suspend`.

#### Implementation tasks

1. Extract terminal session setup into a dedicated struct.
2. Implement `Drop` to best-effort restore terminal state.
3. Make `run` use the guard.
4. Make `with_terminal_suspend` temporarily drop/suspend and then restore via the same abstraction.
5. Preserve the original action error when both action and restore fail.
6. Add tests for setup/restore command ordering and suspend/resume behavior.

### Files likely touched

- `trueflow/src/commands/tui.rs`

## Phase 4: resize and focus polish

### 4A: resize coalescing

#### Problem

`crossterm` documents that resize events can arrive in batches. The TUI currently rerenders one-for-one on every resize event.

#### Recommendation

Drain a short burst of immediately available resize events and redraw only once using the last observed size.

#### Implementation tasks

1. Add a helper to coalesce resize bursts.
2. Use it in both the scope selector and main app loop.
3. Add tests for repeated resize events collapsing to a single rerender request.

### 4B: focus change support

#### Status

Optional.

#### Potential benefit

If enabled, `FocusLost` / `FocusGained` could be used to:

- pause speed-read timers
- flush deferred settings more conservatively
- trigger a redraw on refocus

#### Recommendation

Only do this if there is a clear UX need after the other phases land.

## Proposed commit breakdown

1. `tui: add bracketed paste support to note editor`
2. `tui: align repeat-key handling with keyboard event flags`
3. `tui: probe keyboard enhancement support`
4. `tui: fail early without a tty`
5. `tui: guard terminal session restore`
6. `tui: coalesce resize bursts`
7. optional: `tui: handle terminal focus changes`

## Validation strategy

For each phase:

1. add focused unit tests in `trueflow/src/commands/tui.rs`
2. if behavior is user-visible enough, add an integration or e2e test
3. run targeted `cargo test` first
4. run `env TMPDIR=/tmp nix develop -c just check` before each commit

## Sources used

- `crossterm` event module docs
- `crossterm` `supports_keyboard_enhancement()` docs
- `crossterm` `IsTty` docs
- `crossterm` `event-read.rs` example

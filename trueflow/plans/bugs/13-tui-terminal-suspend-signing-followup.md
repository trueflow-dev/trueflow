# Issue #13: TUI terminal suspend follow-up for signing and other interactive paths

## Current behavior

As of this cleanup:

- `src/commands/mark.rs` owns the suspend-policy decision via `TerminalSuspendRequirement`.
- `src/commands/tui.rs` delegates to `mark::terminal_suspend_requirement_from_workdir()` instead of re-deriving the rule itself.
- The current policy is still conservative:
  - `user.signingkey` configured -> `TerminalSuspendRequirement::Required`
  - no signing key configured -> `TerminalSuspendRequirement::NotRequired`
- `mark::run()` signs records only in the `Required` case, so the TUI suspend policy and mark signing behavior are now structurally aligned.

This is safer than the prior state because the TUI no longer has a separate hand-rolled copy of the signing/suspend rule.

## What is known

From the current code structure:

- The only obviously interactive subprocesses in the mark path are the GPG calls in `src/commands/mark.rs`:
  - `gpg --detach-sign --armor`
  - `gpg --armor --export`
- Everything else in `mark::run()` appears non-interactive:
  - loading git config
  - building the review record
  - computing block state
  - appending to the store
- We can therefore justify making the suspend *decision path* explicit and shared.

## What is not yet known

We do **not** yet have enough structural evidence to safely claim when a configured signing environment can avoid a TUI suspend.

In particular, we have not established:

- when `gpg --detach-sign` will require terminal or pinentry access vs. when it can complete headlessly
- whether `gpg --export` is always safe/non-interactive across supported environments
- how agent/pinentry configuration changes behavior:
  - `gpg-agent`
  - `pinentry`
  - `GPG_TTY`
  - loopback pinentry
  - cached passphrases
  - smartcard / hardware-token flows
- whether there are any other future mark backends that might introduce interactive behavior without going through the same policy surface

Because of those unknowns, we should **not** weaken the conservative signed-action behavior yet.

## Recommended next step

Refactor `mark::run()` into explicit phases so the TUI can eventually suspend only around the actually interactive portion, if that becomes justified.

Suggested shape:

1. Extract a pure/non-interactive preparation phase, e.g.:
   - load runtime config
   - build `Record`
   - compute whether signing is needed
2. Extract signing behind a small interface, e.g. a `Signer` trait or `SigningBackend` enum.
3. Keep store append outside the signing backend.
4. Add a TUI-facing entry point that can do:
   - prepare without suspend
   - suspend only around signing/export if signing is required
   - resume and append afterward

That would narrow the suspend boundary without making any claim that signed flows are non-interactive.

## Recommended tests / experiments

### Code-level tests

Add tests that prove boundary placement, not environment-specific GPG behavior:

- unsigned path does not invoke the suspend wrapper
- signed path invokes the suspend wrapper exactly around the signing backend
- record append still happens after resume
- failures during signing still restore the terminal cleanly

This likely requires dependency injection for:

- the signing backend
- the suspend wrapper
- store append

### Environment experiments

Before changing signed behavior, manually test a small matrix:

- signing key with cached passphrase
- signing key with uncached passphrase
- loopback pinentry enabled/disabled
- `GPG_TTY` present/absent
- hardware token / smartcard if supported
- terminal multiplexer (`tmux`) vs plain terminal

For each case, record:

- whether suspend is actually necessary
- whether failures are recoverable without corrupting the TUI
- whether pinentry appears in the correct place

## Safe change boundary for the next patch

Safe:

- more tests
- internal refactors that separate prepare/sign/append phases
- dependency injection that makes suspend boundaries testable
- narrowing the boundary from `whole mark::run()` to `signing phase only`, **if** the refactor preserves the current conservative decision (`signed => suspend`)

Not yet justified:

- skipping suspend for signed actions based on heuristics about local GPG setup
- claiming pinentry or signing is headless-safe in general
- changing the policy based only on anecdotal environment behavior

# Issue #13: TUI terminal suspend follow-up for signing and other interactive paths

## Current behavior

The TUI now avoids terminal suspend for signing-enabled actions when GPG can sign non-interactively.

Action execution policy:

- unsigned action:
  - run `mark` directly
  - do not leave raw mode / alternate screen
- signed action with cached passphrase, no passphrase, GUI pinentry, or otherwise non-interactive GPG path:
  - first try `mark` with non-interactive signing
  - do not leave raw mode / alternate screen if that succeeds
- signed action that cannot complete non-interactively:
  - fall back to the previous conservative terminal suspend path
  - run normal interactive signing while the TUI is suspended
  - restore the TUI afterward
- non-signing failures after the non-interactive attempt do not retry under suspend, avoiding duplicate action attempts after unrelated failures.

The non-interactive signing attempt runs GPG with:

```text
gpg --batch --no-tty --pinentry-mode error --detach-sign --armor
```

Public-key export runs without terminal access:

```text
gpg --batch --no-tty --armor --export
```

GPG stderr is captured and returned in the error instead of being written directly to the terminal.

## Remaining deliberate suspend behavior

Terminal suspend remains as a fallback only after non-interactive signing fails. This keeps support for setups that genuinely need terminal-mediated pinentry, hardware-token prompts, or other interactive GPG behavior.

The fallback is deliberately broad: if the non-interactive GPG signing/export phase fails, the TUI retries the action through the existing suspended interactive path. That preserves compatibility while eliminating the common flash for cached/non-interactive signing.

## Validation coverage

Code-level tests cover:

- unsigned / no-suspend path runs directly
- signed path skips suspend when non-interactive signing succeeds
- signed path falls back to suspend after non-interactive signing failure
- non-signing failures do not trigger a suspended retry
- non-interactive signing failure classification

PTY smoke coverage verifies:

- a signing-enabled TUI approval with a fake non-interactive `gpg` does not leave alternate screen during approval
- the only alternate-screen leave is the final TUI exit

## Still worth manual testing

Manual environment coverage is still useful for confidence across local GPG setups:

- cached passphrase
- uncached passphrase
- GUI pinentry
- curses pinentry with `GPG_TTY`
- loopback pinentry
- hardware token / smartcard
- tmux vs plain terminal

Expected result:

- cached/non-interactive paths do not flash the TUI
- genuinely interactive paths may still suspend, but only after the non-interactive attempt fails

# trueflow

![trueflow logo](./design/trueflow.jpg)

![trueflow TUI screenshot](./tui.png)

Trueflow is an experimental semantic code review tool.

It turns files or diffs into reviewable **blocks**, lets you review those blocks
in a CLI/TUI/Emacs workflow, and stores review state in an append-only flat file
database, e.g. `.trueflow/reviews.jsonl`.

## What it does

- scans a working tree or revision range into semantic review `blocks` (a
  `block` is some semantic unit of content, e.g. `Method`, `Struct`,
  `CodeParagraph`)
- lets you approve, reject, or comment on `blocks`
- stores review records in `.trueflow/reviews.jsonl`
- exports review feedback for an agent or other automation
- TODO: auto-integration with agents as you review

## Current model

- The canonical review unit is a **block**, not a textual diff hunk.
- Runtime config lives in `trueflow.toml`.

Still some rough edges.

- The current public CLI field name is still `fingerprint`.
- Diff fingerprints and content-addressed block identities both exist today and
  are not fully unified yet.

## Quick start

```sh
# Launch the TUI. The main way to use trueflow.
trueflow tui

# Review current changes as JSON. Machine-readable, suitable for integrations.
trueflow review --json


# Inspect and split a block
trueflow inspect --fingerprint <fp> --split

# Export review feedback
trueflow feedback --format xml
```

## Filter and scope review

```sh
# Review only functions
trueflow review --all --only function --json

# Exclude gaps and comments from feedback output
trueflow feedback --exclude gap --exclude comment

# Launch the TUI scoped to one file
trueflow tui --target file:src/lib.rs

# Scope the TUI to a revision range with additional filtering
trueflow tui --target rev:abc1234..def5678 --only function --exclude comment
```

Block kinds are case-insensitive and match the semantic kinds shown in JSON
output.

## Runtime config

Trueflow looks for a `trueflow.toml` file in the current directory or any parent
folder. It applies defaults for `review` and `feedback` unless overridden by CLI
flags.

```toml
[review]
only = ["function", "struct"]
exclude = ["gap", "comment"]

[feedback]
exclude = ["gap"]

[tui.speed_read]
enabled = true
default_wpm = 320
default_chunk_words = 2
```

See `trueflow.example.toml` for the default settings.

## Interfaces

### TUI

Main review actions:

- `a` approve
- `x` reject
- `c` comment
- `s` split
- `r` toggle speed-reading
- `q` quit

### Emacs

The Emacs frontend provides a Magit-like status view and focused review flow.
Key actions include approve/reject/comment/split/refresh/review-start.

## Feedback and metadata

`trueflow feedback` exports review history for reuse by an agent or another
consumer.

The current public CLI/API field name for a review target is still
`fingerprint`. That currently coexists with separate diff fingerprints, so the
identity surface is not fully unified yet.

Review records can also carry metadata such as reviewer identity and review
labels.

## Development

```sh
nix develop -c just check
nix develop -c just coverage
```

    The coverage report is written to `trueflow/target/llvm-cov/html/index.html`.

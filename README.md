# trueflow

<img src="./design/trueflow.jpg" alt="trueflow logo" width="341">

![trueflow TUI screenshot](./tui.png)

Trueflow is a semantic local code review tool.

Website: <https://trueflow.dev>

It lets you review existing repository content and diffs as semantic **blocks**
instead of raw diff hunks. It is built for local CLI/TUI/Emacs workflows and
stores review state in an append-only flat file database, e.g.
`.trueflow/reviews.jsonl`.

## What it does

- reviews existing code and whole repositories, not just diffs
- scans a working tree or revision range into semantic review `blocks` (a
  `block` is some semantic unit of content, e.g. `Method`, `Struct`,
  `CodeParagraph`)
- presents review targets in a stable priority order so higher-priority
  material appears first
- lets you approve, reject, or comment on `blocks`
- stores review records in `.trueflow/reviews.jsonl`
- exports review feedback for agents and other automation

## Current model and status

- The canonical review unit is a **block**, not a textual diff hunk.
- Review targets are presented in a stable priority order.
  - Today that includes heuristics like tests before library code before main
    entrypoints, and higher-priority block kinds before lower-priority ones
    within a file.
  - The goal is a practical review invariant: if you stop early, you have seen
    the highest-priority material first according to the tool's review-order
    heuristics.
- Runtime config lives in `trueflow.toml`.
- Current website-distributed binary support is Apple Silicon macOS and Linux x86_64.

Still some rough edges.

- The current public CLI field name is still `fingerprint`.
- Diff fingerprints and content-addressed block identities both exist today and
  are not fully unified yet.

## Install

For current install instructions and release downloads, see:

- <https://trueflow.dev/install/>

Current website-distributed binary support: Apple Silicon macOS and Linux x86_64.

### Alternative install paths

With Nix:

```sh
nix profile install github:trueflow-dev/trueflow
```

With Cargo:

```sh
# Install Rust and Cargo first: https://rustup.rs
git clone git@github.com:trueflow-dev/trueflow.git
cd trueflow
cargo install --path trueflow --locked
```

`cargo install` usually puts the `trueflow` binary in `~/.cargo/bin`, so make
sure that directory is on your `PATH`.

## Official language support

Trueflow always falls back to semi-smart text processing when official
semantic/AST blocking is not available yet. That fallback still gives you
usable review units via paragraphs, sentences, code chunks, comments, and the
usual review-priority heuristics.

In the matrix below:

- `✅` = official semantic/structured blocking today
- `🚧` = detection/fallback works today, but official semantic/AST blocking is
  still coming soon
- `Subblock split` means language-specific `inspect --split` behavior, not only
  generic code fallback

Review priority heuristics apply across all review targets, including fallback
modes.

### Official semantic / structured blocking today

| Language | Semantic / AST blocks | Subblock split | Complexity scoring | TUI highlight |
| --- | --- | --- | --- | --- |
| Rust | ✅ | ✅ | ✅ | ✅ |
| Swift | ✅ | ✅ | ✅ | ✅ |
| Emacs Lisp | ✅ | ✅ | ✅ | ✅ |
| JavaScript | ✅ | ✅ | ✅ | ✅ |
| TypeScript | ✅ | ✅ | ✅ | ✅ |
| Java | ✅ | ✅ | ✅ | ✅ |
| Kotlin | ✅ | ✅ | ✅ | ✅ |
| C# | ✅ | ✅ | ✅ | ✅ |
| Python | ✅ | ✅ | ✅ | ✅ |
| Ruby | ✅ | ✅ | ✅ | ✅ |
| PHP | ✅ | ✅ | ✅ | ✅ |
| Shell | ✅ | — | ✅ | ✅ |
| C | ✅ | ✅ | ✅ | ✅ |
| Zig | ✅ | ✅ | — | — |
| Lua | ✅ | ✅ | — | — |
| Dart | ✅ | ✅ | — | — |
| Scala | ✅ | ✅ | — | — |
| Haskell | ✅ | ✅ | — | — |
| OCaml | ✅ | ✅ | — | — |
| Elixir | ✅ | ✅ | — | — |
| Clojure | ✅ | ✅ | — | — |
| SQL | ✅ | — | — | — |
| YAML | ✅ | ✅ | — | — |
| JSON | ✅ | ✅ | — | — |
| HTML | ✅ | ✅ | — | — |
| CSS | ✅ | ✅ | — | — |
| Markdown | ✅ | ✅ | — | — |
| TOML | ✅ | ✅ | — | — |
| Nix | ✅ | ✅ | — | ✅ |

### Fallback / heuristic support today, official semantic / AST blocking coming soon

| Language | Semantic / AST blocks | Subblock split | Complexity scoring | TUI highlight |
| --- | --- | --- | --- | --- |
| Go | 🚧 | — | — | ✅ |
| C++ | 🚧 | — | — | ✅ |
| Just | 🚧 | — | — | ✅ |
| Text / Org | 🚧 | ✅ | — | — |

Notes:

- Most `✅` code languages use tree-sitter-backed structural blocking.
- Markdown, TOML, and some config/data formats use custom structured splitting
  instead of a full AST.
- `🚧` languages still work today through heuristic or text-oriented fallback,
  but they are not yet at the same official semantic/AST support level.

## Website infra (`trueflow.dev`)

The repo also contains Terraform-compatible OpenTofu configuration for the
static website and download host at `trueflow.dev`.

What it stands up:

- one **private S3 bucket** for site and download artifacts
- one **CloudFront distribution** in front of that bucket, using OAC
- one **ACM certificate** for `trueflow.dev` and `www.trueflow.dev`
- **Route53 alias records** for apex + `www`
- a small **CloudFront Function** to redirect `www` to the apex host and rewrite
  clean paths like `/about/` and `/install/`

What it does **not** stand up:

- no new Route53 hosted zone
- no EC2 / containers / Lambda app backend
- no public S3 website hosting
- no databases or other stateful services

It reuses the existing public Route53 hosted zone for `trueflow.dev`.

From the repo root:

```sh
nix develop
./scripts/deploy-public-site.sh
```

That one command will:

- run `tofu init`, `tofu fmt -check`, `tofu validate`, and `tofu apply`
- upload `website/`
- package the Apple Silicon macOS binary artifact
- upload `/download/` artifacts

For the common fast path after infra is already up:

```sh
./scripts/deploy-public-site-fast.sh
```

That keeps the safety checks (`tofu init`, `fmt`, `validate`) but skips
`tofu apply` before uploading website + download artifacts.

If you want to run the steps manually instead:

```sh
cd infra/terraform
tofu init
tofu fmt -check
tofu plan
# inspect the plan carefully
# when ready:
tofu apply
cd ../..
./scripts/deploy-website.sh
```

Note: on local Darwin right now, the flake-pinned Nix `aws` binary hangs
during startup, so the dev shell intentionally relies on your existing ambient
`aws` instead of shadowing it.

To package and upload the current Apple Silicon macOS binary separately:

```sh
./scripts/package-macos-release.sh
./scripts/deploy-downloads.sh .trueflow/release-artifacts/v0.1.0
```

To package and smoke-test a native Linux x86_64 musl release on Linux x86_64:

```sh
./scripts/package-linux-release.sh
./scripts/smoke-test-release.sh .trueflow/release-artifacts/v0.1.0/trueflow-v0.1.0-x86_64-unknown-linux-musl.tar.gz
```

That Linux release flow uses `nix build .#release` as the build source of truth.
Website installer and install-page wiring for Linux is a follow-up; for now this
produces local release artifacts that can also be uploaded under `/download/`.

To upload a different artifact directory later:

```sh
./scripts/deploy-downloads.sh path/to/release-artifacts
```

For more detail, see `infra/terraform/README.md`.

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

# Scope the review to an entire directory subtree
trueflow review --target dir:website --json
trueflow tui --target dir:trueflow/src

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

[tui]
# disabled|old_new
# default: disabled
# diff_line_numbers = "old_new"

[tui.keybinds]
scroll_up = "k"
scroll_down = "j"
prev = "h"
next = "l"
parent = "P"
child = "C"
approve = "a"
note = "c"
toggle_view = "m"
speed_read = "r"
root = "g"
quit = "q"

[tui.speed_read]
enabled = true
default_wpm = 320
default_chunk_words = 2
```

See `trueflow.example.toml` for the default settings.

## Interfaces

### TUI

Main review actions:

- In the root view, `j`/`k` and Up/Down move the selection through the visible file/dir list
- In the root view, `l`, Right, Enter, and `c` open the selected item; `h`, Left, and `p` are back/leftward actions and are a no-op at the repository root
- Outside the root view, `j`/`k` and Up/Down scroll code line-by-line
- `PageUp`, `PageDown`, `Space`, `Home`, and `End` scroll by page or jump to the top/bottom of the current code view
- Outside the root view, `h`/`l` and Left/Right move to the previous/next semantic sibling
- `p`/`c` move to the semantic parent/child
- `a` approve
- `n` add a note (`Enter` submits, `Ctrl+J` inserts a newline, and the TUI requires note text before submit)
- `m` toggle diff/source
  - diff-mode line numbers are disabled by default; set `[tui] diff_line_numbers = "old_new"` to restore the old/new gutter
- `r` toggle speed-reading
- `g` jump to root
- `q` quit

To override the default TUI keys, add a `[tui.keybinds]` section to
`trueflow.toml`:

```toml
[tui.keybinds]
scroll_up = "i"
scroll_down = "m"
prev = "j"
next = "l"
parent = "u"
child = "o"
approve = "y"
note = "e"
toggle_view = "v"
speed_read = "s"
root = "z"
quit = "x"
```

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
# Default local gate (tests, lint, fmt)
nix develop -c just check

# Faster no-test local gate
nix develop -c just check-fast

# Broad local code verification path
nix develop -c just check-code

# Separate Nix packaging verification
nix develop -c just check-packaging

# Verify the host-default flake package directly
nix develop -c just nix-check

# Optional: verify the explicit release/static flake package
nix develop -c just nix-check-release

# Optional: rerun buildRustPackage's package-level test phase explicitly
nix develop -c just nix-check-with-tests

# Capture timing breakdowns for the current gate definitions
nix develop -c just measure-check
nix develop -c just measure-check-fast
nix develop -c just measure-check-code

# Generate a coverage report
nix develop -c just coverage
```

`just check` is the default local gate: tests, lint, and format checks.
`just check-fast` keeps the faster compile-only path for cases where you want a quicker no-test loop.
`just check-code` runs the broader lib/bin/test/example code path plus audit, docs, and coverage, while still excluding benches.
That code-focused path builds only this crate's docs (not dependency docs) and measures coverage for the main lib/bin/test target set.
Bench targets are opt-in and only run through `just bench`.
Normal compile/test/lint/doc/coverage recipes enable `tui-test-support` so the hidden vt100/PTy TUI regression harness keeps compiling in ordinary local gates.
`just check-packaging` runs host-default Nix packaging verification separately from the regular code checks.
`just nix-check` validates the host-default flake package (`native` on Darwin, release/static on Linux).
`just nix-check-release` is the explicit release/static package build.
`just nix-check-with-tests` is the explicit opt-in path for rerunning crate tests inside the host-default Nix package build.
Timing artifacts are written under `.trueflow/measurements/`.

    The coverage report is written to `trueflow/target/llvm-cov/html/index.html`.

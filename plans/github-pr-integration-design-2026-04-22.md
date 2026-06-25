# GitHub PR Integration Design

Status: local draft, not checked in
Date: 2026-04-22

## Summary

Implement GitHub pull request integration for trueflow with two primary user flows:

1. `trueflow tui --target pr:...`
   - resolve a GitHub PR
   - fetch its commits locally without checking out or switching the worktree
   - run the existing trueflow review flow commit-by-commit, oldest to newest

2. `trueflow feedback --pr ...`
   - collect note-bearing trueflow feedback recorded against commits in that PR
   - remap those per-commit comments onto the PR head diff when possible
   - post one aggregated GitHub PR review on the PR head
   - skip unmappable comments with explicit warnings
   - avoid reposting already-delivered records

Internally, trueflow remains commit-centric. GitHub posting is a projection layer from per-commit review records onto the final PR head diff.

## Decisions already made

These are fixed for v1 unless explicitly changed.

- Internal trueflow review/feedback remains **per-commit**.
- GitHub posting is **aggregated on PR head**.
- PR review fetch is **fetch-only**, with **no checkout** and **no worktree switching**.
- The primary UX is `tui --target pr:...`.
- GitHub integration uses **`gh` CLI** for now.
- All `gh` usage goes behind a **small GitHub client abstraction** so we can replace it later.
- Must support both **github.com** and **GitHub Enterprise-style hosts**.
- GitHub export always posts a **COMMENT** review event.
- If a comment cannot be placed on the PR head diff, trueflow **skips it and warns**. It must not silently drop it.
- Delivery is **idempotent by default**. Re-running export must not repost already-delivered trueflow records.
- This design should not depend on the long-term existence of a local `Rejected` note type. If `Rejected` is removed later, the integration still works because export is driven by note-bearing records and GitHub review event is always `COMMENT`.

## Goals

- Add a PR-scoped review target for TUI.
- Reuse existing commit-scoped review machinery as much as possible.
- Persist enough comment anchor metadata to support correct GitHub inline posting.
- Post one aggregated GitHub review on the PR head.
- Keep correctness conservative: map only when confidence is high, otherwise skip with warnings.
- Preserve append-only trueflow history and avoid mutating existing review records.
- Be host-aware for private/internal/company GitHub installations.

## Non-goals for v1

- No automatic checkout, branch switching, or worktree creation.
- No attempt to support PR targets uniformly across every command. In v1, PR target support is primarily for `tui`.
- No attempt to synthesize GitHub `APPROVE` or `REQUEST_CHANGES` review events.
- No guarantee of rename-aware forward remapping in v1.
- No fallback body section that pastes skipped comments into the GitHub review. Unmappable comments are skipped and reported locally.
- No backwards-perfect placement for old trueflow records created before the new anchor metadata exists.

## Why this design is needed

Trueflow already has the core pieces for commit-scoped review:

- `ReviewTarget::Revision(...)`
- `ScopePreset::Commit`
- commit-scoped diff review in `trueflow/src/commands/review.rs`
- note/comment persistence in the review store

However, current stored records do not preserve enough information to faithfully project comments from an earlier commit onto the final PR head diff. Today we persist:

- repo revision
- path hint
- line hint
- visible comment scope
- visible comment context

That is enough for history export and best-effort context recovery, but not enough for reliable GitHub inline comment placement, especially for:

- diff-mode comments on removed lines
- comments made on earlier PR commits that are later edited again
- mixed diff-view selections

So the main new technical requirement is: **store stronger comment anchors and add a conservative forward-remapping layer**.

## User-facing UX

## PR reference syntax

Support these forms:

- `pr:11`
- `pr:owner/repo/11`
- full URL, e.g. `https://github.com/owner/repo/pull/11`
- full enterprise URL, e.g. `https://github.company.com/owner/repo/pull/11`

Resolution rules:

- `pr:11` infers host/owner/repo from the current repository remote.
- `pr:owner/repo/11` infers host from the current repository remote unless a full URL is used.
- Full URLs carry their own host.
- If the current local repository does not correspond to the PR base repository, trueflow should fail early with a clear error.

## TUI review flow

Example:

```sh
trueflow tui --target pr:11
trueflow tui --target pr:jmqd/trueflow/11
trueflow tui --target https://github.company.com/jmqd/trueflow/pull/11
```

Behavior:

1. Resolve the PR via GitHub.
2. Fetch required commit objects into hidden refs locally.
3. Expand the PR into an ordered sequence of commit-scoped review requests.
4. Launch trueflow review for the oldest PR commit.
5. After each commit review recap, advance to the next commit until done or until the user exits.
6. Show PR/commit progress in the UI, e.g. `PR #11 commit 2/5`.

Additional review filters such as `--only` and `--exclude` apply to each commit in the sequence.

## Feedback export / posting flow

Example:

```sh
trueflow feedback --pr pr:11
trueflow feedback --pr https://github.com/jmqd/trueflow/pull/11
trueflow feedback --pr pr:jmqd/trueflow/11 --dry-run
```

Behavior:

1. Resolve the PR and fetch missing objects.
2. Collect candidate trueflow records associated with commits in that PR.
3. Ignore records already delivered for this PR.
4. Remap eligible per-commit comment anchors onto the final PR head diff.
5. Post one GitHub PR review using event `COMMENT`.
6. Persist delivered record IDs.
7. Print a summary including posted count and skipped warnings.

Records that cannot be mapped inline on the PR head diff are skipped and reported locally.

## High-level architecture

## New/expanded modules

### `trueflow/src/github.rs`

Owns the GitHub abstraction layer.

Responsibilities:

- parse/normalize PR refs and PR URLs
- resolve current repo remote host/owner/repo
- query PR metadata through `gh`
- post pull request reviews through `gh`
- be the only layer that knows GitHub API and `gh` argument details

Suggested top-level types:

```rust
pub struct PullRequestRef {
    pub host: String,
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

pub struct PullRequestMetadata {
    pub pr: PullRequestRef,
    pub title: String,
    pub base_ref: String,
    pub base_sha: CommitId,
    pub head_ref: String,
    pub head_sha: CommitId,
    pub commits: Vec<PullRequestCommit>,
}

pub struct PullRequestCommit {
    pub sha: CommitId,
    pub summary: String,
}

pub struct GitHubReviewDraft {
    pub body: String,
    pub comments: Vec<GitHubInlineComment>,
    pub skipped: Vec<SkippedGitHubComment>,
}

pub struct GitHubInlineComment {
    pub path: RepoPath,
    pub line: u32,
    pub side: GitHubCommentSide,
    pub start_line: Option<u32>,
    pub start_side: Option<GitHubCommentSide>,
    pub body: String,
}
```

Suggested trait:

```rust
pub trait GitHubClient {
    fn resolve_pull_request(&self, pr: &PullRequestRef) -> Result<PullRequestMetadata>;
    fn post_pull_request_review(
        &self,
        pr: &PullRequestRef,
        head_sha: &CommitId,
        draft: &GitHubReviewDraft,
    ) -> Result<PostedGitHubReview>;
}
```

Initial implementation: `GhGitHubClient`.

### `trueflow/src/github_delivery.rs`

Owns idempotent local delivery tracking.

Responsibilities:

- persist which trueflow record IDs have already been delivered to which PR
- answer `already_posted(record_id)` efficiently
- append newly-posted IDs after a successful post

### `trueflow/src/comment_anchor.rs` or store-local equivalent

Owns persisted anchor types and remapping logic.

Responsibilities:

- define stored source/diff anchors
- define projected head anchors
- remap anchors forward through a PR commit chain

### Existing modules that will change

- `trueflow/src/cli.rs`
- `trueflow/src/targets.rs`
- `trueflow/src/commands/tui.rs`
- `trueflow/src/commands/feedback.rs`
- `trueflow/src/store.rs`
- `trueflow/review_record.schema.json`
- likely `trueflow/src/main.rs`
- possibly `trueflow/src/vcs.rs` for helper functions used by remapping/fetch support

## Detailed design

## 1. GitHub abstraction and PR resolution

Implement a thin host-aware GitHub layer backed by `gh`.

### Host-awareness

The abstraction must carry host explicitly, not assume `github.com`.

This matters for:

- URL parsing
- `gh --hostname <host> ...`
- matching current repo remote against the PR repository

### `gh` usage strategy

Use `gh` for:

- PR metadata lookup
- review posting

Likely commands:

- `gh pr view ... --json ...`
- `gh api repos/{owner}/{repo}/pulls/{number}/reviews ...`

Implementation detail should stay isolated inside `GhGitHubClient` so the rest of the code does not depend on command strings or JSON response shapes.

### Current repo matching

When resolving a PR:

- parse current repo remote URL (`origin` first, then fallback candidates if needed)
- compare host/owner/repo against the resolved PR base repo
- fail early if they do not match

This prevents reviewing/posting against the wrong repository with a local clone that cannot support the required git object lookups.

## 2. Fetch strategy

Trueflow should fetch PR-related objects locally without switching branches or worktrees.

### Requirements

- no checkout
- no branch switch
- no mutation of user worktree contents
- hidden refs only

### Strategy

Fetch these into hidden refs under a trueflow-owned namespace, for example:

- `refs/trueflow/pr/<number>/head`
- `refs/trueflow/pr/<number>/base`

If helpful, also fetch per-commit objects directly by SHA so commit-scoped historical review can resolve them locally.

Potential implementation approaches:

1. Fetch GitHub PR refs directly where supported.
2. Fall back to fetching explicit SHAs returned by PR metadata.

The fetch helper should be conservative and validate that required SHAs are available locally after the fetch.

## 3. TUI PR review sequence

We should reuse existing commit review logic rather than inventing a separate PR review engine.

### CLI shape

In v1:

- `tui --target pr:...` is supported.
- non-TUI commands using `--target pr:...` should return a clear error unless explicitly implemented.
- `feedback --pr ...` is a separate path, not a target expansion.

### Review expansion

A PR target expands to an ordered list of commit review requests:

```rust
ReviewRequest::Targets(vec![ReviewTarget::Revision(commit_sha)])
```

ordered oldest to newest.

### TUI launch model

Add a small launch-plan abstraction, for example:

```rust
enum ReviewLaunchPlan {
    Single(CliReviewRequest),
    Sequence(Vec<CliReviewRequest>),
}
```

`Sequence` is only needed for PR review in v1.

### UI metadata

Display PR review progress in the TUI state so the user sees:

- PR number/title
- current commit index / total commits
- current commit SHA/summary

The existing recap flow should offer to continue to the next PR commit until the sequence is exhausted.

## 4. Persist stronger comment anchors

This is the key store/schema change.

### New record field

Add a new optional field to `Record`:

```rust
pub comment_anchor: Option<CommentAnchor>
```

This should be added in `store.rs` and the JSON schema, with a record version bump.

### Proposed anchor model

```rust
pub enum CommentAnchor {
    Source(SourceCommentAnchor),
    Diff(DiffCommentAnchor),
}

pub struct SourceCommentAnchor {
    pub revision: CommitId,
    pub path: RepoPath,
    pub start_line: u32, // 0-based inclusive
    pub end_line: u32,   // 0-based exclusive
}

pub struct DiffCommentAnchor {
    pub revision: CommitId,
    pub path: RepoPath,
    pub rows: Vec<DiffCommentAnchorRow>,
}

pub struct DiffCommentAnchorRow {
    pub kind: DiffLineKind,
    pub old_line: Option<u32>, // 1-based git diff line numbering
    pub new_line: Option<u32>, // 1-based git diff line numbering
}
```

This model is intentionally conservative:

- source-mode comments keep a source range in the reviewed revision
- diff-mode comments preserve the actual visible diff rows, including removed-line information

### Why row-level diff capture

Row-level diff capture preserves more truth than collapsing immediately to one line range. It supports:

- comments on removed lines
- comments on added lines
- comments on mixed diff views, which may later prove unmappable

Then the exporter can decide whether the stored diff selection is representable on the PR head diff.

### Existing fields to keep

Keep these existing fields:

- `comment_scope`
- `comment_context`

They are still useful for:

- local display
- best-effort historical context
- warnings
- debugging mismatches

`comment_anchor` is additive and becomes the preferred source for GitHub export.

### TUI capture rules

When the user creates a note/comment:

- in source mode, store `CommentAnchor::Source`
- in diff mode, store `CommentAnchor::Diff`

Anchor metadata must be derived from the current scope/revision/path, not from the current mutable worktree.

### Unsupported nodes

Comments attached to nodes without a meaningful file/range anchor, such as repository root or directory nodes, should keep recording as trueflow records if desired, but GitHub export will treat them as unplaceable and warn/skip.

## 5. Forward remapping from commit anchors to PR head

This is the core projection layer.

### Inputs

- PR ordered commits: `[c1, c2, ..., head]`
- a trueflow record tied to revision `ci`
- a stored `CommentAnchor`

### Output

Either:

- a valid GitHub PR-head inline comment target
- or an explicit unmappable reason

### Conservative correctness rule

If trueflow cannot map a record to the PR head diff with high confidence, it must skip that record and report a warning.

Wrong placement is worse than under-placement.

### Source-anchor remapping

For `SourceCommentAnchor`:

1. Start with `(path, start_line, end_line)` in revision `ci`.
2. For each consecutive commit pair from `ci -> ci+1 -> ... -> head`, translate the source range forward through that step's file diff hunks.
3. If the range is deleted, split, or becomes ambiguous, stop and mark unmappable.
4. Once projected to PR head source lines, check whether the resulting range is still part of the PR head diff relative to the PR base.
5. If yes, emit a GitHub head comment anchor.
6. If not, skip with warning.

### Diff-anchor remapping

For `DiffCommentAnchor`:

1. Start from the stored diff rows in revision `ci`.
2. Translate the row positions forward through later commit diffs.
3. Reduce the translated rows to a GitHub-representable inline target only if all relevant rows collapse to:
   - one path
   - one side (`LEFT` or `RIGHT`)
   - one contiguous line or multiline range supported by GitHub review comments
4. Verify that this target exists on the PR head diff.
5. If not representable or not present on the head diff, skip with warning.

### Unmappable reasons

Use explicit reason categories such as:

- `MissingCommentAnchor`
- `LegacyRecordWithoutAnchor`
- `UnsupportedTargetKind`
- `MissingPath`
- `MissingCommitObject`
- `PathRenamedUnsupported`
- `RangeDeletedByLaterCommit`
- `AmbiguousLineTranslation`
- `MixedDiffSides`
- `NotPresentInPrHeadDiff`

These reasons should surface in local warnings and test assertions.

### Rename handling

V1 should be conservative.

If the remapper cannot confidently follow a path across a rename, it should skip with a `PathRenamedUnsupported` warning rather than guessing.

Rename-aware remapping can be a later improvement.

## 6. GitHub review draft building

Add a dedicated planner that builds a GitHub review draft from trueflow records plus PR metadata.

### Candidate record selection

For v1, a record is a candidate for GitHub posting if all of these are true:

- it belongs to a commit in the PR commit set
- it has a non-empty note/body to post
- it has not already been delivered for this PR
- it can be remapped to the PR head diff

This deliberately decouples GitHub export from the local verdict vocabulary. If `Rejected` disappears later, the posting behavior remains unchanged.

### Aggregation

Build one `GitHubReviewDraft` for the PR head.

- `event = COMMENT`
- `comments = [all successfully remapped inline comments]`
- `body = machine-generated summary text only`
- skipped/unmappable records are not pasted into the review body

Suggested body content:

- generated-by marker
- PR/head SHA
- count of inline comments posted
- maybe count of skipped comments, but without embedding the skipped comment text

### Duplicate record IDs

The planner should deduplicate by trueflow record ID before posting.

## 7. Delivery ledger / idempotency

Add a repo-local delivery ledger, separate from the existing feedback cursor.

Suggested file:

- `.trueflow/github_delivery.json`

Suggested keying:

- host
- owner
- repo
- PR number

Each PR entry stores:

- delivered trueflow record IDs
- timestamps
- optionally GitHub review IDs for diagnostics

### Important behavior

Only successful posts are recorded as delivered.

Skipped/unmappable records are **not** recorded as delivered, so they may be retried in later runs if conditions change.

This preserves idempotency for posted records while allowing later attempts for previously-unmappable ones.

## 8. CLI changes

## `tui`

Keep the existing shared target flag, but add PR-target parsing support for the TUI path.

Examples:

```sh
trueflow tui --target pr:11
trueflow tui --target pr:jmqd/trueflow/11
```

## `feedback`

Add a dedicated `--pr` option.

Examples:

```sh
trueflow feedback --pr pr:11
trueflow feedback --pr https://github.com/jmqd/trueflow/pull/11 --dry-run
```

Suggested v1 behavior:

- `--pr` is mutually exclusive with `--target`
- `--pr` implies GitHub posting mode, not plain XML/JSON export
- add `--dry-run` to print the planned review payload and warnings without posting

If preserving plain export and posting under one command feels too overloaded, we can split later, but v1 can keep it under `feedback` as requested.

## Implementation plan

Follow a TDD-first path for each phase where practical.

## Phase 1: GitHub abstraction and PR reference parsing

Implement:

- `PullRequestRef` parsing
- current-repo remote parsing and inference
- `GitHubClient` trait
- `GhGitHubClient` metadata lookup

Tests:

- parse `pr:11`
- parse `pr:owner/repo/11`
- parse github.com URL
- parse enterprise URL
- infer host/owner/repo from SSH and HTTPS remotes
- reject malformed PR refs
- reject repo mismatch between local clone and target PR

## Phase 2: Fetch helpers

Implement:

- hidden-ref fetch logic
- local object availability validation

Tests:

- fetch command construction
- successful hidden-ref fetch in temp repo fixture
- clear error when required objects are still unavailable

## Phase 3: TUI PR sequence support

Implement:

- PR target handling for `tui`
- launch-plan abstraction for single vs sequence review
- commit-sequence advancement and progress metadata

Tests:

- PR metadata expands to ordered commit review requests
- sequence runs oldest to newest
- user exit stops sequence cleanly
- `--only` / `--exclude` are preserved for every commit in the sequence

## Phase 4: Persist comment anchors

Implement:

- new `comment_anchor` field in `Record`
- schema/version bump
- source-mode anchor capture in TUI
- diff-mode row anchor capture in TUI

Tests:

- record serialize/deserialize round trip
- schema acceptance for new record format
- source anchor captured for source-mode comment
- diff anchor rows captured for diff-mode comment on added lines
- diff anchor rows captured for diff-mode comment on removed lines
- legacy records without `comment_anchor` still load

## Phase 5: Remapper

Implement:

- source range forward translation through commit chain
- diff-row forward translation through commit chain
- projection to GitHub head comment target
- unmappable reason reporting

Tests:

- source range survives small edits and lands on head diff
- source range deleted later becomes unmappable
- removed-line diff anchor that still exists on head diff maps to `LEFT`
- mixed diff-side anchor is skipped as unmappable
- legacy record without anchor yields warning
- rename edge case warns and skips in v1

## Phase 6: GitHub review draft builder

Implement:

- candidate record selection
- remap-to-inline planning
- body generation
- skip reporting
- deduplication by record ID

Tests:

- note-bearing record becomes one inline comment
- record with no note is ignored
- duplicate record IDs collapse to one comment
- unmappable record appears in skipped diagnostics, not inline comments
- draft is always `COMMENT` event

## Phase 7: Delivery ledger and posting path

Implement:

- local delivery store
- `feedback --pr`
- `--dry-run`
- `GhGitHubClient::post_pull_request_review`

Tests:

- dry-run does not mutate delivery ledger
- successful post records delivered IDs
- rerun skips already-delivered records
- mixed batch posts only newly-undelivered records
- failed post does not record delivery

## Phase 8: Docs and environment

Implement:

- README updates
- CLI help updates
- `flake.nix` dev-shell inclusion of `gh`

Tests/checks:

- help text assertions where practical
- `just check`

## Test strategy

Use a layered test plan.

### Unit tests

For:

- PR ref parsing
- remote URL parsing
- anchor serialization
- line translation through hunks
- draft planning
- delivery ledger

### Integration tests

Use temp git repos and mocked `gh` command behavior to test:

- PR resolution
- fetch-only review flow setup
- delivery idempotency
- end-to-end draft generation from stored records

### TUI tests

Reuse existing TUI test support to verify:

- PR sequence launch and advance
- source-mode anchor capture
- diff-mode anchor capture

### Manual verification checklist

Before considering the feature complete:

1. Review a synthetic PR with 3 commits via `tui --target pr:...`.
2. Leave comments on:
   - unchanged-forward source lines
   - added lines
   - removed lines
   - a comment that becomes stale after a later commit
3. Run `feedback --pr ... --dry-run` and inspect:
   - correct inline plan for mappable comments
   - warnings for stale/unmappable comments
4. Run actual `feedback --pr ...` against a test PR.
5. Re-run and verify no duplicate posting.
6. Add one new note and verify only the new record posts.

## Risks and mitigations

## Risk: forward remapping is subtly wrong

Mitigation:

- keep the remapper conservative
- require explicit representability on the PR head diff
- skip on ambiguity
- add focused line-translation tests

## Risk: legacy records cannot be placed

Mitigation:

- warn explicitly with reason `LegacyRecordWithoutAnchor`
- do not guess too aggressively
- accept that only new records get full-fidelity posting

## Risk: enterprise host handling is incomplete

Mitigation:

- make host explicit in all PR-ref and client types
- test github.com plus at least one enterprise-style host string
- isolate `gh --hostname` usage inside the client

## Risk: idempotency breaks after partial failures

Mitigation:

- only mark records as delivered after a successful GitHub API response
- keep delivery persistence separate from planning

## Acceptance criteria

The feature is complete when all of the following are true:

- `trueflow tui --target pr:...` reviews PR commits sequentially without checkout/worktree switching.
- trueflow stores strong comment anchors for new source-mode and diff-mode comments.
- `trueflow feedback --pr ...` posts one aggregated GitHub `COMMENT` review on PR head.
- Mappable comments appear inline on the PR head diff.
- Unmappable comments are skipped with explicit warnings.
- Re-running `feedback --pr ...` does not repost already-delivered record IDs.
- The implementation works for both github.com and enterprise-style hosts when `gh` is configured for them.

## Follow-up work after v1

Possible follow-ups once v1 is stable:

- rename-aware forward remapping
- support `review --target pr:...` outside the TUI path
- better PR-progress UI inside recap and root selector views
- richer dry-run output with grouped warning summaries
- optional posting of skipped comments as a non-inline summary artifact outside GitHub review comments
- replacing `gh` with a native client if dependency or portability becomes a concern

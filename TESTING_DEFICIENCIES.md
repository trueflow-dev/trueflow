# TESTING_DEFICIENCIES

## Feedback filtering regressions (#17)

### Missing boundary-condition coverage for `--since`
- We had tests that covered filtering strictly *after* a timestamp, but not the inclusive boundary case.
- Result: `timestamp > threshold` slipped through even though user-facing semantics needed `>=`.
- Missing test layer: end-to-end CLI behavior test for exact-boundary timestamps.
- Follow-up: add systematic boundary tests for all supported `--since` forms (`unix ts`, relative duration, RFC3339, `last`).

### Missing end-to-end coverage for historical tree drift
- We did not test feedback export after the reviewed block stopped linking to the current worktree tree.
- Result: recent/historical reviews could silently disappear from `trueflow feedback` even though they were still in `reviews.jsonl`.
- Missing test layer: end-to-end behavior tests where a review is recorded on one revision and the file changes afterward.
- Follow-up: every history-sensitive export path should have at least one “tree drift after review” e2e test.

### Missing cursor-granularity tests for `--since last`
- We did not test multiple records created at the same timestamp boundary.
- Result: a timestamp-only cursor could drop same-second records or force awkward semantics.
- Missing test layer: end-to-end cursor tests covering same-second additions and repeat exports.
- Follow-up: test cursor behavior at identical timestamps and across repeated invocations.

### Missing record-centric range semantics tests for `rev:A..B`
- Existing tests exercised historical content loading, but not whether feedback records were filtered by `repo_ref.revision` membership in the requested range.
- Result: out-of-range records on matching paths could leak into exports, while in-range records could be omitted after later tree drift.
- Missing test layer: end-to-end behavior tests asserting exact record inclusion/exclusion by revision range.
- Follow-up: for every revision-scoped export path, test both:
  - in-range vs out-of-range record membership
  - later-drift historical context resolution

### Synthetic test records were too unrealistic in key places
- Several tests used fabricated records without realistic `path_hint`, `line_hint`, or `repo_ref.revision` values.
- Result: path/revision-sensitive behavior was under-specified and easier to get wrong without tests failing.
- Missing test layer: fixture helpers that default to realistic record shapes, plus e2e tests that prefer real CLI-produced records where practical.
- Follow-up: improve test helpers so historical/export tests naturally include realistic metadata unless a test explicitly opts out.

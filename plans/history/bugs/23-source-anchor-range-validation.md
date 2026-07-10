# Issue #23: Validate source-anchor ranges before line translation

Status: ready  
Date: 2026-07-10  
Baseline commit: `9a98914698c4`

## Problem

Pull-request feedback treats the numeric bounds in a persisted `SourceCommentAnchor` as trustworthy. A source anchor uses a zero-based, half-open range `[start_line, end_line)`, but `translate_source_anchor_to_head` converts it immediately to one-based line numbers with:

```rust
(anchor.start_line.saturating_add(1)..=anchor.end_line).collect::<Vec<_>>()
```

The function checks only that `end_line > start_line` and that the path exists at the anchored revision. It does not establish that either bound is within the file stored at `anchor.revision` before materializing and later iterating the range.

Consequences:

- On a one-line source blob, `{ "start_line": 0, "end_line": 2 }` is allowed past the shape check even though line 2 does not exist.
- `{ "start_line": 0, "end_line": 4294967295 }` creates the inclusive iterator `1..=u32::MAX` and asks `Vec<u32>` to hold 4,294,967,295 entries. The payload alone is 17,179,869,180 bytes, approximately 16 GiB, before allocator overhead.
- An invalid persisted record can therefore exhaust memory before feedback can classify and skip that record.
- The current `saturating_add` and endpoint clamping in line translation hide arithmetic boundaries instead of proving that a line number is representable.

The required invariant is:

> Before any source-anchor line range is materialized or iterated, `0 <= start_line < end_line <= anchored_blob_line_count` must be established. Thereafter, source-range translation keeps only bounded endpoints and constant-size iteration state. Its work is bounded by the anchored blob and subsequent real diff inputs, and its additional memory is independent of the untrusted numeric span.

Invalid anchors must be rejected as explicit per-record skips. They must never be accepted, truncated, clamped to the file, or converted into a giant allocation.

## Evidence

- `trueflow/src/store.rs:229-238` derives `Deserialize` for `SourceCommentAnchor` and stores `start_line` and `end_line` as `u32`. The `schemars(range(min = 0))` attributes describe the generated schema; they do not compare the two fields or validate either one against repository content.
- `trueflow/src/store.rs:917-943` parses each JSONL line into `serde_json::Value` and then into `Record`. A nonnegative value through `u32::MAX` is structurally valid, so a persisted `SourceCommentAnchor` with `end_line: 4294967295` reaches the feedback path unchanged rather than being counted as a malformed record.
- `trueflow/src/store.rs:1032-1050` shows that `FileStore::read_history` reads the JSONL database and returns successfully deserialized records. This is the untrusted persistence boundary the regression coverage must exercise.
- `trueflow/src/commands/feedback.rs:761-793` builds a feedback plan record by record. The nested result from `map_record_to_github_comment` already distinguishes a local skip reason from an operational `anyhow::Error`, so an invalid anchor can be isolated without aborting valid records.
- `trueflow/src/commands/feedback.rs:846-879` sends source anchors through `translate_source_anchor_to_head`, then checks whether the translated range is visible in the pull-request head diff. Source-bound validation therefore belongs before translation, not in the later GitHub-diff visibility check.
- `trueflow/src/commands/feedback.rs:1038-1103` checks `end_line <= start_line`, checks only path existence, and then collects every line in `start_line + 1..=end_line` into `Vec<u32>`. The pull-request commit lookup occurs after that collection.
- `trueflow/src/commands/feedback.rs:1388-1399` implements `path_exists_in_revision` with a tree-entry lookup. Existence does not prove that the entry is a source blob or establish its line count.
- `trueflow/src/vcs.rs:483-501` demonstrates the repository's existing gix convention for file access: look up the tree entry, reject a tree, load the object, and call `try_into_blob()`. The feedback implementation should follow that convention rather than shelling out or reading the worktree, because the anchor is tied to a historical revision.
- `trueflow/src/commands/tui.rs:4635-4649` constructs source anchors from selected zero-based source-line indices and uses `last + 1` as the exclusive end. This confirms the half-open source-anchor contract that validation must preserve.
- Existing controls in `trueflow/src/commands/feedback.rs:1858-1885` and `2057-2116` cover a valid one-line head anchor and a valid source line translated after a rename and insertion. They must remain green.
- `PullRequestFeedbackSkipReason` and its `Display` implementation at `trueflow/src/commands/feedback.rs:351-384`, plus outcome rendering at `1436-1486`, provide the existing user-visible skip classification path.

## Reproduction

Use a one-line head file and a source anchor at that head revision.

1. Safe baseline reproduction: create an anchor with `start_line = 0` and `end_line = 2`, run it through `build_pull_request_feedback_plan`, and assert that the record is classified as an invalid source range. At the baseline this assertion is red: the two-element range is allocated and the invalid bound is not identified against the one-line blob. This case is intentionally small and cannot trigger OOM.
2. Persistence-boundary reproduction: append a record whose source anchor has `start_line = 0` and `end_line = u32::MAX` to a fixture `FileStore`, reload the database, extract the deserialized anchor, and pass it only to the new non-allocating bounds validator with the anchored blob's line count. Assert that deserialization preserves `u32::MAX` and validation returns the invalid-range skip. This test is safe even when the assertion fails because it never constructs or collects the numeric range.
3. Do **not** execute the baseline `translate_source_anchor_to_head` implementation end to end with `0..u32::MAX`: its current `Vec<u32>` collection is the approximately 16 GiB allocation being fixed. The small integration red test establishes that validation is wired into translation; the pure `u32::MAX` validator test establishes the upper boundary without making the regression suite itself an OOM hazard.

Initial safe red command from the repository root:

```sh
cd trueflow && cargo test --lib commands::feedback::tests::build_pull_request_feedback_plan_skips_source_anchor_outside_anchored_blob -- --exact
```

The test must fail by assertion with the baseline behavior, not by allocation failure, timeout, panic, or process termination.

## Root cause

`SourceCommentAnchor` is a persisted coordinate, not a trusted in-memory slice. Serde can establish only that both JSON numbers fit in `u32`; it cannot establish relational or repository-dependent facts such as `start_line < end_line` or `end_line <= line_count(anchor.revision, anchor.path)`. The generated JSON schema likewise cannot inspect a Git blob.

`translate_source_anchor_to_head` currently conflates three separate operations:

1. checking the anchor's numeric shape;
2. resolving the path through later commits; and
3. expanding and translating every one-based line number.

Because the source blob is never loaded at the anchor revision, operation 1 is incomplete. Because operation 3 begins with `collect::<Vec<_>>()`, both memory use and the first expensive action are controlled directly by persisted numbers. `saturating_add`, saturating cursor increments, and clamping the final mapped value to `u32::MAX` further prevent arithmetic failure from being represented explicitly.

Validation cannot be moved into `SourceCommentAnchor` deserialization: the deserializer does not have the repository, revision object, or source path content. The correct trust boundary is `translate_source_anchor_to_head`, immediately after resolving the anchor revision/path to its blob and before creating an iterator over the range.

## Implementation plan

1. **Add safe red contract tests in `trueflow/src/commands/feedback.rs::tests`.**
   - Add `build_pull_request_feedback_plan_skips_source_anchor_outside_anchored_blob` using `pull_request_fixture_with_file_contents` with one-line base/head blobs and a head-revision anchor `[0, 2)`. Assert no draft comment or staged record ID, exactly one skip with the new invalid-range reason, and a rendered `Skipped record ...` line naming the invalid source range.
   - Add small shape cases for `[1, 1)` and `[2, 1)` and expect the same invalid-range classification rather than `RangeDeletedByLaterCommit`.
   - Add `source_anchor_range_validation_rejects_persisted_u32_max_without_expansion`. Persist and reload a `Record` through `FileStore`, match the reloaded `CommentAnchor::Source`, confirm `end_line == u32::MAX`, load/count the one-line anchored blob, and call only the non-allocating bounds-validation entry point. Assert the invalid-range result. This can initially be compile-red against the named validator, but it must never call the baseline expanding translator.
   - Keep the tests deterministic and repository-local by using the existing temporary Git fixtures, `FileStore::for_root`, and `review_record` helpers. Do not introduce a memory limit, subprocess crash expectation, huge fixture, timeout, or allocator hook.

2. **Introduce an explicit invalid-anchor skip in `trueflow/src/commands/feedback.rs`.**
   - Add `PullRequestFeedbackSkipReason::InvalidSourceAnchorRange` and a stable `Display` message such as `source anchor range is outside the anchored source file`.
   - Use this reason for empty/inverted source ranges, an exclusive end beyond the anchored blob's logical line count, and any checked conversion needed to form the one-based inclusive range.
   - Preserve the nested-result contract: invalid persisted data is `Ok(Err(InvalidSourceAnchorRange))`, so the bad record is skipped while other records continue. Missing pull-request commits, later deletion, ambiguous translation, and unsupported path remapping retain their existing skip variants. Revision parsing, object loading, and repository I/O failures remain outer `Err(anyhow::Error)` failures; they are operational failures, not malformed user coordinates.
   - Do not reuse `RangeDeletedByLaterCommit` for a range that never existed in the anchored source. That reason remains reserved for a valid source range/path that disappears during later history translation.

3. **Load and validate the anchored source blob before creating a range in `trueflow/src/commands/feedback.rs::translate_source_anchor_to_head`.**
   - Resolve the anchor's commit membership before expensive source work, then resolve `anchor.revision` to its commit/tree, look up `anchor.path`, and load the entry as a blob using the same gix pattern as `vcs::file_state_for_path_in_tree`.
   - Replace the initial `path_exists_in_revision` call with a focused helper such as `source_blob_line_count_in_revision`. Return `None` for a missing/non-file entry and preserve the existing missing-source-path skip; propagate genuine revision/tree/object read failures.
   - Count logical lines directly from `blob.data` without UTF-8 conversion or a per-line allocation: count `b'\n'` and add one only when nonempty bytes do not end in `b'\n'`. Thus `b""` has 0 lines, `b"one"` and `b"one\n"` have 1, and `b"one\ntwo\n"` has 2. Use checked counting/conversion rather than saturation.
   - Validate the persisted zero-based half-open range as `start_line < end_line` and `usize::try_from(end_line) <= blob_line_count` (or an equivalently checked comparison). Only after this succeeds may `start_line.checked_add(1)` produce the one-based inclusive first line. Never subtract the untrusted bounds to size a container.
   - Keep this check against the blob at `anchor.revision`, not the worktree and not the pull-request head: a range may be valid at the anchored commit and legitimately be deleted or shifted later.

4. **Replace source-line `Vec<u32>` expansion with a bounded streaming representation in `trueflow/src/commands/feedback.rs`.**
   - Introduce a small endpoint type such as `InclusiveSourceLineRange { first: u32, last: u32 }`, constructed only after blob validation. It must hold two numbers, not one element per source line.
   - For each later commit with hunks, iterate `first..=last` lazily and fold mapped lines into only `(first_mapped, previous_mapped, last_mapped)`. A missing mapping returns `RangeDeletedByLaterCommit`; a mapped value that is not exactly `previous.checked_add(1)` returns `AmbiguousLineTranslation`; an arithmetic/conversion overflow returns an explicit skip rather than saturating or clamping.
   - Store the resulting first/last endpoints for the next commit and for `TranslatedSourceAnchor`. Do not collect the input or mapped output into `Vec<u32>`.
   - Update `translate_old_line_to_new_line_strict` as needed so checked cursor/mapped-line overflow is distinguishable from a deleted line. Never use `.min(u32::MAX)`, `saturating_add`, or duplicate clamped endpoints to make an unrepresentable result look valid. It is acceptable to map representational overflow to the existing `AmbiguousLineTranslation` skip if that choice is named and tested; it must not be silently accepted.
   - Leave `diff_anchor_lines_for_side` and left-side diff-anchor translation unchanged. Their vectors are bounded by persisted diff rows and are not the source-anchor numeric expansion at issue.

5. **Add valid and historical controls in `trueflow/src/commands/feedback.rs::tests`, then run the focused smoke tests.**
   - Pure validator controls: accept `[0, 1)` for one-line blobs and `[0, 3)`/`[1, 3)` for three-line blobs; reject any nonempty range for an empty blob; cover both trailing-newline and unterminated-final-line counts.
   - Valid single-line integration control: retain `build_pull_request_feedback_plan_maps_head_source_anchor_to_inline_comment` and its GitHub right-side line 1 result.
   - Valid multiline integration control: anchor a contiguous multiline range that is in bounds at an earlier revision, insert a line before it in a later commit, and assert both translated endpoints and the resulting GitHub `start_line`/`line` remain contiguous and correct.
   - Deleted-line control: anchor an in-bounds source line at an earlier revision, delete that line later, and assert `RangeDeletedByLaterCommit`. This proves source-bound validation does not reclassify a real later deletion as malformed input.
   - Keep the persisted `u32::MAX` test on the validator/blob-loading boundary only. The one-line `[0, 2)` integration test proves that translation calls validation before iteration, while source review confirms there is no source-range collection left.

6. **After the focused smoke is green, perform cleanup and full validation.**
   - Remove the obsolete source-anchor `Vec<u32>` construction and any imports/helpers used only by it. Keep `is_contiguous` if the diff-anchor paths still use it.
   - Ensure helper names state zero-based half-open versus one-based inclusive semantics, and keep all arithmetic checked at the conversion boundary.
   - Do not alter `SourceCommentAnchor`'s serialized shape or `store.rs`; existing beta records remain readable and are validated when repository context is available.
   - Run the complete focused command set below, then the repository's final `just check` gate.

### Ordered affected files and symbols

1. `trueflow/src/commands/feedback.rs`
   - `PullRequestFeedbackSkipReason` and its `Display` implementation
   - `translate_source_anchor_to_head`
   - new anchored-blob line-count/range-validation helper
   - new constant-size inclusive source-line range and streaming translation helper
   - `translate_old_line_to_new_line_strict` only as required to replace saturation/clamping with checked failure
   - `commands::feedback::tests` fixtures and regression/control tests

`trueflow/src/store.rs`, `trueflow/src/commands/tui.rs`, and `trueflow/src/vcs.rs` are evidence and contract references only. No persisted schema, anchor producer, or general VCS API change is required.

## Verification and validation

Run all commands from the repository root.

1. Establish the safe red result before the source fix:

   ```sh
   cd trueflow && cargo test --lib commands::feedback::tests::build_pull_request_feedback_plan_skips_source_anchor_outside_anchored_blob -- --exact
   ```

   Expected before the fix: an assertion failure showing that the one-line blob's `[0, 2)` anchor is not classified as `InvalidSourceAnchorRange`. Any OOM, abort, timeout, or giant allocation means the test was designed incorrectly.

2. Run the bounds and persistence tests after the fix:

   ```sh
   cd trueflow && cargo test --lib commands::feedback::tests::source_anchor_range_validation -- --nocapture
   ```

   This filter must include the persisted `u32::MAX` case, empty/one-line/multiline bounds, invalid shapes, and trailing-newline semantics. The `u32::MAX` case must complete normally with bounded memory because it calls validation, not source-line expansion.

3. Run source-history translation controls:

   ```sh
   cd trueflow && cargo test --lib commands::feedback::tests::translate_source_anchor_to_head -- --nocapture
   ```

   This filter must include the valid multiline shift and valid-then-deleted controls.

4. Re-run existing source-anchor behavior:

   ```sh
   cd trueflow && cargo test --lib commands::feedback::tests::build_pull_request_feedback_plan_maps_head_source_anchor_to_inline_comment -- --exact
   cd trueflow && cargo test --lib commands::feedback::tests::build_pull_request_feedback_plan_remaps_renamed_source_anchor_then_translates_lines -- --exact
   ```

5. Run the focused feedback unit-test group to catch nearby skip rendering and diff-anchor regressions:

   ```sh
   cd trueflow && cargo test --lib commands::feedback::tests -- --nocapture
   ```

6. Behavioral/manual review:
   - Inspect the invalid one-line plan and confirm it has zero comments, zero staged record IDs, one `InvalidSourceAnchorRange` skip, and a user-visible `Skipped record <id>: ...` line.
   - Inspect the persisted-boundary test and confirm `FileStore::load_database` yielded `end_line == u32::MAX`; the test must not obtain safety by making serde reject or clamp that valid `u32` value.
   - Confirm no path from `translate_source_anchor_to_head` calls `.collect::<Vec<u32>>()` or reserves capacity from `end_line - start_line`.
   - Confirm the anchored blob is loaded from `anchor.revision`, and that validation occurs before constructing or iterating `first..=last`.
   - Confirm a valid range deleted by a later commit is still reported as `RangeDeletedByLaterCommit`, while operational gix failures still propagate as command errors.

7. Final repository gate:

   ```sh
   just check
   ```

## Acceptance criteria

- A source anchor is accepted only when its zero-based half-open bounds satisfy `start_line < end_line <= line_count` for the blob at exactly `anchor.revision` and `anchor.path`.
- A one-line blob with `[0, 2)` produces no GitHub comment and is explicitly skipped as an invalid source-anchor range.
- A persisted `SourceCommentAnchor { start_line: 0, end_line: u32::MAX }` remains deserializable as a `u32` record but is rejected by blob-aware validation without constructing, reserving, or iterating a range of that numeric size.
- The `u32::MAX` regression test is safe when red: it invokes only constant-memory validation and cannot reproduce the approximately 16 GiB allocation inside the test process.
- Empty, zero-width, reversed, past-EOF, trailing-newline, and unterminated-final-line boundaries have deterministic tests.
- Valid one-line and multiline anchors still translate to the same one-based GitHub lines.
- An anchor valid at its source revision but deleted later remains `RangeDeletedByLaterCommit`; noncontiguous later mappings remain `AmbiguousLineTranslation`.
- Invalid persisted bounds are per-record skips. Repository/revision/tree/blob I/O failures remain command errors. No invalid bound is accepted, clamped, saturated, or mislabeled as a later deletion.
- Source-range translation stores only endpoints and constant-size fold state; it does not allocate a `Vec` proportional to the anchor's numeric span.
- Work and memory are bounded by actual anchored blob size and real later diff/history inputs, not by an out-of-bounds persisted integer. Extra range-state memory is `O(1)`; blob/diff memory is proportional to repository content already being inspected.
- Existing diff-anchor behavior, source rename behavior, persisted JSON shape, and the final `just check` gate remain green.

## Non-goals and risks

### Non-goals

- Silently accepting, truncating, or clamping an invalid source anchor.
- Rejecting `u32::MAX` at serde/schema level merely because it is large; validity depends on the anchored blob and must be decided with repository context.
- Changing the serialized `SourceCommentAnchor` shape, migrating existing JSONL records, or adding a compatibility shim. The project is beta and the existing shape is sufficient.
- Adding an arbitrary fixed maximum comment span. A large range is valid when the anchored source actually contains it; the source blob is the semantic bound.
- Changing diff-anchor row representation, left-side GitHub comment mapping, rename detection, pull-request membership rules, or head-diff visibility policy.
- Reworking the general gix/VCS loading API or reading the worktree instead of the anchored revision.
- Broad optimization of hunk lookup beyond removing the untrusted `Vec<u32>` expansion.

### Risks and mitigations

- **Trailing-newline off-by-one:** counting every newline and adding a final line only for nonempty non-newline-terminated content avoids a phantom line after a trailing newline. Cover empty, terminated, and unterminated blobs directly.
- **Coordinate-system confusion:** retain explicit names/documentation for zero-based half-open persisted bounds and one-based inclusive GitHub/mapping bounds; test both endpoints of a multiline range.
- **Misclassification:** validate against the anchor revision before walking later commits. Tests must distinguish never-valid ranges from valid ranges deleted later.
- **Arithmetic saturation:** use checked addition/conversion for `start + 1`, streaming contiguity, cursor movement, and mapped line numbers. Treat failure as a named skip; never clamp.
- **Regression-test OOM:** never send the persisted `u32::MAX` anchor through the known-expanding baseline translator. Pair a small-span end-to-end red test with a maximum-value pure validator test.
- **Large but valid files:** validation scans blob bytes and streaming translation may visit each actually anchored line, but it retains constant-size range state. This is intentionally bounded by real source size rather than a forged out-of-bounds end value.
- **Additional blob access:** replacing an existence-only tree lookup with blob loading is necessary to establish the security boundary. Avoid an extra content copy and count directly over `blob.data`.

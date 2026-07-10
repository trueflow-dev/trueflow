# Issue #22: Feedback cursor gap-safe atomicity

Status: ready  
Date: 2026-07-10  
Baseline commit: 9a98914698c4

## Problem

`feedback --since last` currently treats the greatest timestamp in the records selected by the current invocation as proof that every older record has already been exported. That implication is false when target, revision, approval, or block-kind filters exclude records. For example, exporting a selected record at timestamp `2000` advances the cursor past an unselected record at timestamp `1000`; a later broader invocation then permanently suppresses the older record.

The cursor file is also updated with a truncating `fs::write`. A process failure, short write, full filesystem, or concurrent reader can expose an empty or partial file. Concurrent writers can overwrite a more complete cursor with a stale candidate because there is no stable lock or read/merge/write transaction.

The replacement must provide these observable semantics:

- Record ID, not timestamp, is the exactly-once identity. Distinct records created in the same second remain distinct.
- A record not selected by one successful `--since last` export remains eligible for a later export under a different selection, even if newer selected records were already exported.
- A record is removed from `--since last` eligibility only after it appears in a successfully rendered local feedback result and the cursor commit succeeds.
- Across successful, serialized `--since last` invocations, every record that eventually passes a selection is emitted exactly once. Explicit `--since all` and timestamp modes keep their current non-consuming behavior.
- Every cursor read observes either the complete previous state or the complete replacement state. No failure may expose an empty or partially serialized cursor.

## Evidence

- `trueflow/src/commands/feedback.rs::build_feedback_cursor_from_entries` examines only `FeedbackEntry.reviews`, retains only IDs at the maximum exported timestamp, and therefore has no representation for records excluded before grouping.
- `trueflow/src/commands/feedback.rs::build_feedback_cursor_after_entries_export` compares the exported and previous maximum timestamps. A newer exported timestamp replaces the previous cursor wholesale; only equal-timestamp IDs are unioned. It cannot retain an older filtered gap.
- `trueflow/src/feedback_export.rs::collect_feedback_entries` resolves all records but applies `record_matches_since` before allowed-revision, path-selection, approval, and block filters. The records rejected by those later predicates never reach either cursor builder.
- `trueflow/src/feedback_export.rs::record_matches_since` accepts only timestamps greater than the cursor timestamp, plus IDs not yet listed at exactly the cursor timestamp. Any unexported record below the timestamp is irrecoverable through `--since last`.
- `trueflow/src/feedback_export.rs::read_feedback_cursor` treats an empty existing file as no cursor, accepts the legacy timestamp-only format, and parses the current unversioned timestamp/ID object. `write_feedback_cursor` uses `fs::write` directly on `feedback.cursor`, so replacement is neither locked nor atomic.
- `trueflow/src/commands/feedback.rs::run` renders entries before calling `write_feedback_cursor_update`, which is the correct success boundary to preserve: a render error must not consume entries. However, the current `FeedbackCursorUpdate` owns only a path and a precomputed cursor, so it cannot retain a lock or merge against the state on disk at commit time.
- `trueflow/src/commands/feedback.rs::run_prepared_pull_request_feedback_with_filters` currently loads `ReviewDatabase` before `filter_pull_request_feedback_records` resolves and reads a `last` cursor. With a checkpointed prefix, a concurrent local exporter could commit checkpoint `N` after the PR path captured only `N - 1` records, producing a legitimate prefix-validation failure. A read-only `last` operation must therefore acquire and retain the shared cursor guard before taking its database snapshot.
- `trueflow/src/store.rs::JsonlStoreBackend::append` opens `reviews.jsonl` with append mode and an exclusive `fs2` file lock, while `read_history` preserves the logical physical-record order returned by the JSONL parser. Timestamps are not the append-order contract. This supports a cursor checkpoint over the append-only logical record sequence. `fs2`, `sha2`, and `uuid` are already direct dependencies in `trueflow/Cargo.toml`; no new persistence dependency is needed.
- `trueflow/tests/bug_regressions.rs::test_feedback_since_last_cursor_ignores_filtered_out_records` establishes that filtered-out records must remain available, but its selected record is older (`1000`) than the filtered record (`2000`). That order does not exercise the destructive cursor advance.
- `trueflow/src/feedback_export.rs::tests::collect_feedback_entries_filters_cursor_ids_once_per_export` covers equal-timestamp IDs only under the old timestamp model, and `read_feedback_cursor_supports_legacy_timestamp_format` explicitly protects a format that this clean cutover must remove.

## Reproduction

1. Create two review records for different files in append order: `newer-selected` for `src/a.rs` with timestamp `2000`, followed by `older-filtered` for `src/b.rs` with timestamp `1000`.
2. Run `trueflow feedback --format json --since last --target file:src/a.rs`. The output contains only `newer-selected`.
3. The current implementation writes a cursor whose timestamp is `2000`.
4. Run `trueflow feedback --format json --since last` without the target filter.
5. Expected: the output contains `older-filtered`, and does not repeat `newer-selected`. Current result: the output is empty because `record_matches_since` rejects timestamp `1000`.
6. Run the same sequence with both timestamps equal and distinct IDs. The second ID must remain independently exportable, and neither ID may reappear after it has been committed.

The persistence failure is independently reproducible by interrupting the current direct write after truncation: an empty file is interpreted as `All`, causing re-export, while a partial JSON object makes every later `--since last` fail to parse. Two direct writers can also replace each other's state without detecting regression.

## Root cause

The timestamp cursor encodes a contiguous chronological prefix, but filtered export creates holes and record timestamps are neither unique nor guaranteed to follow append order. `build_feedback_cursor_from_entries` cannot distinguish “older and already delivered” from “older and excluded by this query,” so no merge of its current output can recover the missing information.

Use the append-only logical record sequence as the compact frontier and explicitly store only unresolved holes. The new cursor schema is:

```json
{
  "version": 1,
  "checkpoint": {
    "record_count": 2,
    "last_record_id": "older-filtered",
    "record_ids_sha256": "<lowercase hex SHA-256>"
  },
  "pending_record_ids": ["older-filtered"]
}
```

Represent this with versioned, `deny_unknown_fields` Serde structs such as `FeedbackCursor` and `FeedbackCursorCheckpoint`, not loosely typed JSON. Define the digest exactly as SHA-256 over a domain prefix for cursor version 1 followed by each logical record ID in append order, each framed by its big-endian `u64` UTF-8 byte length. This avoids concatenation ambiguity and lets the reader verify that the stored checkpoint is still a prefix of the current logical database.

The chosen invariant is:

> For a cursor checkpoint of `N` logical records, the count, tail ID, and prefix digest must match `ReviewDatabase.records()[..N]`. Every ID in `pending_record_ids` occurs exactly once in that prefix and has not been successfully exported by `--since last`. Every other ID in that prefix has been successfully exported. Every record at index `N` or later is unobserved and eligible. Pending IDs are sorted and unique in serialized state.

This state does not retain an unbounded list of delivered IDs. It is bounded by the number of unresolved filtered gaps plus a constant-size checkpoint. If a filter permanently excludes an ever-growing set, the pending set necessarily grows: preserving arbitrary future eligibility requires representing those gaps, so dropping or bounding them would reintroduce data loss. A prefix mismatch, duplicate record identity, checkpoint count overflow, inconsistent empty checkpoint (`record_count == 0` with a tail ID, or nonzero count without one), or pending ID outside the checkpoint must produce an actionable error and leave the cursor untouched; it must never guess that unknown records were delivered.

To advance from a valid previous cursor:

1. Start with the prior pending IDs.
2. Add every record ID in the newly observed suffix `records[previous.record_count..]`.
3. Remove exactly the IDs flattened from the entries successfully selected for this export.
4. Advance the checkpoint to the complete database snapshot, recompute its tail ID and prefix digest, then sort and deduplicate the remaining pending IDs.

For no existing cursor, use an empty checkpoint and apply the same algorithm. This makes reversed timestamps and same-second timestamps irrelevant. It also makes progress monotonic: checkpoint count never decreases, an existing pending gap disappears only when that exact ID was exported, and newly observed but unexported IDs are never omitted.

## Implementation plan

1. **Add the observable red regressions before changing cursor code.**
   - In `trueflow/tests/bug_regressions.rs`, rename `test_feedback_since_last_cursor_ignores_filtered_out_records` to `test_feedback_since_last_cursor_keeps_older_filtered_gap` and reverse its timestamps so the first scoped export selects `a-review@2000` and filters out `b-review@1000`. Assert the first output contains only A, the following unfiltered output contains only B, and a third unfiltered invocation is empty. Run this test alone and record that the second assertion fails on the baseline.
   - Add a same-second case with two distinct IDs. Select one ID through its file target, then broaden the query; assert the other ID is emitted once and the selected ID is not repeated.
   - Add an alternating-filter case over at least three records/files: A-only, B-only, A-only again, then unfiltered. Assert each successful result contains only the never-delivered IDs eligible for that query, the repeated A selection is empty, the final broad selection drains the remaining gap, and one more broad selection is empty.
   - Add a deterministic concurrent local-command regression using two threads/processes against the same repository and one unexported record. Start both `--since last` invocations together; assert the combined JSON outputs contain the record exactly once, both commands succeed, the cursor remains parseable, and a final invocation is empty. This test protects the lifetime of the exclusive lock across read, collection, render, and commit, rather than merely testing atomic bytes.

2. **Specify the gap model and atomic store with unit tests while still red.**
   - In `trueflow/src/feedback_export.rs` tests, replace timestamp-cursor expectations with checkpoint/pending expectations. Cover an older pending record below an already delivered newer record, two distinct IDs with the same timestamp, an appended record whose timestamp is lower than the checkpoint tail's timestamp, an empty database, no exported entries, and draining the final pending gap.
   - Replace `read_feedback_cursor_supports_legacy_timestamp_format` with rejection tests for a plain integer, the old unversioned `{timestamp, record_ids_at_timestamp}` JSON, empty existing content, unknown version, inconsistent checkpoint fields, prefix mismatch, and a pending ID outside the checkpoint. Missing `feedback.cursor` must still mean no prior cursor.
   - Add an injected pre-replace failure test through a narrow private test seam immediately after the temporary file has been written and synced but before rename. Seed a valid old cursor, request a newer state, inject the error, and assert the call fails, the target bytes still parse as the exact old cursor, and no temporary file is treated as the cursor.
   - Add concurrent persistence tests: shared readers looping while a writer replaces the cursor may observe only the complete old or complete new schema; two writers must serialize on the stable sidecar lock and the second must advance/merge from the first writer's committed state rather than resurrecting a delivered gap or decreasing the checkpoint.
   - In `trueflow/src/commands/feedback.rs`, add `feedback_cursor_pr_snapshot_acquires_guard_before_database_load`. Exercise the same private `load_pull_request_feedback_snapshot_with` helper used by production, injecting only the database-loader closure. Inside that closure, assert an exclusive sidecar `try_lock` fails, proving the shared guard was acquired before loading; keep the returned snapshot alive and assert the writer remains blocked through filtering, then drop it and assert the writer can acquire. This tests actual PR orchestration order rather than only cursor-guard primitives.

3. **Replace the timestamp model in `trueflow/src/feedback_export.rs`.**
   - Replace `FeedbackCursor { timestamp, record_ids_at_timestamp }` with the versioned `FeedbackCursor`/`FeedbackCursorCheckpoint` composition above and a constant schema version. Remove the timestamp-only parse branch and all legacy-field compatibility. A present but empty/corrupt/unsupported cursor is an error, not equivalent to no cursor.
   - Add a single checkpoint validator that accepts the current `&[Record]`, performs checked `u64`/`usize` conversions, verifies count/tail/digest and unique IDs, and returns the observed prefix length plus a `HashSet` of pending IDs. Reuse this validator in matching and advancement so two cursor interpretations cannot diverge.
   - Change cursor matching used by `collect_feedback_entries` and `record_matches_since`: `All` remains inclusive of all records, `TimestampInclusive` keeps its existing `record.timestamp >= timestamp` behavior, and `Cursor` matches a record when its original `ResolvedFeedbackRecord.index` is at or beyond `checkpoint.record_count` or its ID is pending. Validate the checkpoint against the complete `records` slice before the per-record loop. Remove the timestamp-ID cache.
   - Add an advancement function that receives the prior cursor (if any), the complete ordered database snapshot, and the set of exported record IDs; union prior gaps with the newly observed suffix, subtract only exported IDs, and build the new checkpoint. It must assert that every exported ID was eligible in the transaction snapshot and must never infer delivery from a timestamp.

4. **Make cursor persistence crash-safe and monotonic in `trueflow/src/feedback_export.rs`.**
   - Use a stable same-directory sidecar such as `feedback.cursor.lock`; never lock the replaceable cursor inode. Open it read/write with `create(true)` and use the existing `fs2::FileExt` dependency. Shared reads acquire a shared sidecar lock. A `FeedbackCursorUpdateGuard` acquires the exclusive sidecar lock, reads/validates the current cursor, and owns the lock until commit or drop.
   - Eliminate the public/direct truncating `write_feedback_cursor` path. The update guard's commit must re-read the current on-disk state while still exclusively locked and call the advancement/merge function with the database snapshot and actually exported IDs. This makes the merge monotonic instead of accepting a stale precomputed whole cursor. No compliant writer can race between that read and replace.
   - Serialize the merged cursor plus a trailing newline before touching the destination. Create a unique `create_new` temporary file in the cursor's parent directory (for example `.feedback.cursor.<uuid>.tmp`), write all bytes, call `sync_all` on the temporary file, atomically rename it over `feedback.cursor`, then open and `sync_all` the parent directory before releasing the lock. Clean up an unrenamed temporary file on error without altering the destination. Use the existing `uuid` dependency for collision-resistant names.
   - Preserve failure semantics explicitly: before rename, the previous cursor remains byte-for-byte intact; after rename, readers see complete new bytes; if parent-directory sync fails, report the error even though a complete monotonic new file may already be visible. A crash around rename may recover old or new state, but never partial state and never a state that marks an unresolved gap delivered.

5. **Move local `--since last` orchestration onto one cursor transaction in `trueflow/src/commands/feedback.rs`.**
   - Refactor `collect_local_feedback` so it resolves the since mode, acquires the exclusive cursor update guard for `Last`, and only then loads `ReviewDatabase`. Derive `FeedbackSinceFilter::Cursor` from the guard's snapshot or `All` when no cursor exists. Loading after lock acquisition prevents a waiter from combining a newer cursor with a stale database snapshot.
   - Replace `FeedbackCursorUpdate { path, cursor }` with an update value that owns the guard, the ordered snapshot/checkpoint input, and the flattened exported record IDs. Keep that value in `FeedbackCommandResult`, so the exclusive lock survives collection and rendering.
   - Preserve the existing success boundary in `run` and `collect_feedback_json_values`: render/convert first, then commit. If collection or rendering returns an error, dropping the guard performs no cursor update. Document in code through naming and ownership rather than comments or a compatibility wrapper.
   - Delete `build_feedback_cursor_from_entries` and `build_feedback_cursor_after_entries_export`; selected entries alone cannot build the required state. Replace them with a narrowly named `exported_feedback_record_ids(&[FeedbackEntry]) -> HashSet<String>` collector, and pass its result with the complete ordered record snapshot to the gap-aware advancement function in `feedback_export.rs`. Replace the old timestamp-merge unit tests with checkpoint/pending invariant tests.
   - Refactor `run_prepared_pull_request_feedback_with_filters` through a private `load_pull_request_feedback_snapshot_with` helper that resolves read-only `Last`, acquires a `FeedbackCursorReadGuard`, and only then invokes its database-loader closure. The production closure calls `store.load_database()`; place the returned snapshot in a narrow lexical scope that owns the database, derived filter, and shared guard only through `filter_pull_request_feedback_records`, then release the guard immediately when filtering returns, before plan construction or any GitHub mutation/network call. Other since modes carry no guard. Where a delivery-ledger lock is also present, the fixed acquisition order is ledger then cursor; never reacquire/retain cursor while performing delivery. The PR path must not update the cursor; its delivery ledger remains the delivery authority.

6. **Run a focused smoke after the minimal implementation works, then perform cleanup.**
   - Run the reversed-timestamp regression first, followed by all filtered-cursor regressions and the cursor-model/persistence unit tests listed below.
   - After the smoke is green, remove obsolete timestamp-only fixtures, imports, cache construction, and builder tests; do not leave aliases, migration code, deprecated fields, or dual schema readers. Keep deterministic sorted serialization for reviewable cursor diffs.
   - Re-run the focused commands after cleanup, then run the repository gate. No `Cargo.toml` change, new dependency, user documentation file, or cursor migration is expected.

### Ordered affected files and symbols

1. `trueflow/tests/bug_regressions.rs`
   - `test_feedback_since_last_cursor_keeps_older_filtered_gap` (renamed reversed-timestamp red regression)
   - new same-second, alternating-filter, and concurrent exactly-once regressions
2. `trueflow/src/feedback_export.rs`
   - `FeedbackCursor`, new `FeedbackCursorCheckpoint`, schema version and prefix-digest helpers
   - `FeedbackSinceFilter`, `collect_feedback_entries`, `record_matches_since`
   - `resolve_since_filter`, `read_feedback_cursor`, `write_feedback_cursor` replacement, sidecar-lock/update guard, atomic replace helper
   - cursor model, validation, failure-injection, and concurrency unit tests
3. `trueflow/src/commands/feedback.rs`
   - `FeedbackCommandResult`, `FeedbackCursorUpdate`, `collect_local_feedback`, `write_feedback_cursor_update`
   - removal of `build_feedback_cursor_from_entries` and `build_feedback_cursor_after_entries_export`; new `exported_feedback_record_ids`
   - `run`, `collect_feedback_json_values`, `run_prepared_pull_request_feedback_with_filters`, and `filter_pull_request_feedback_records` (shared guard acquired before the PR database snapshot, retained through filtering, and released immediately afterward)
   - new `load_pull_request_feedback_snapshot_with` and `feedback_cursor_pr_snapshot_acquires_guard_before_database_load`

## Verification and validation

Run from the repository root in this order:

```sh
cd trueflow && cargo test --test bug_regressions test_feedback_since_last_cursor_keeps_older_filtered_gap -- --exact --nocapture
cd trueflow && cargo test --test bug_regressions test_feedback_since_last_cursor -- --nocapture
cd trueflow && cargo test --lib feedback_export::tests::feedback_cursor -- --nocapture
cd trueflow && cargo test --lib commands::feedback::tests::feedback_cursor_pr_snapshot_acquires_guard_before_database_load -- --exact --nocapture
cd trueflow && cargo test --test bug_regressions -- --nocapture
just check
```

Name every new cursor persistence/model unit test with the `feedback_cursor` substring so the third command exercises schema validation, gap advancement, shared-reader/exclusive-writer locking, injected failure, temporary-file replacement, and monotonic concurrent merge. The fourth command specifically exercises the production PR snapshot helper's guard-before-database ordering. Each focused command must report at least one executed test; a zero-test match is a verification failure.

Behavioral checks in the tests must establish all of the following, not merely successful parsing:

- Reversed timestamps: selected `A@2000` is emitted first; filtered `B@1000` is emitted by the later broad query; A never repeats.
- Same-second IDs: distinct IDs at one timestamp are independently pending/delivered.
- Alternating filters: changing A/B/A/unfiltered selections neither loses a gap nor repeats a committed ID.
- Appended backdated record: a record physically after the checkpoint is eligible even when its timestamp is lower.
- Concurrency: concurrent successful `--since last` invocations emit each ID once in aggregate; shared readers parse only complete old/new states; checkpoint progress never regresses and pending gaps are not resurrected.
- PR/read-only snapshot ordering and lifetime: a shared `last` guard covers cursor read, database load, and filtering, so a local writer cannot publish a checkpoint newer than the reader's snapshot; the guard is released immediately after filtering and is not held during plan construction or GitHub calls.
- Failure before replace: the operation returns an error and the exact prior cursor remains readable; a retry can export the still-pending record.
- Render failure: no cursor commit occurs because the update guard is dropped before commit.
- Durability sequence: the tested write path is same-directory temp creation, file sync, atomic rename, and parent-directory sync under the sidecar lock.
- Corruption/prefix mismatch: the command fails without modifying the cursor or silently treating unknown records as delivered.
- Final drain: after every pending/new record has passed a selection once, the next identical or broader `--since last` result is empty.

## Acceptance criteria

- The new versioned cursor schema stores a validated append-sequence checkpoint and sorted unique pending record IDs; it contains no timestamp frontier.
- The checkpoint/pending invariant is enforced on read, match, merge, and write. Delivered history is compacted into the constant-size checkpoint; only unresolved gaps grow.
- The reversed-timestamp regression fails on baseline and passes after the fix: exporting a newer selected record cannot hide an older unselected record.
- Distinct same-second IDs and records appended with lower timestamps remain independently exportable.
- Alternating target/filter selections eventually export every matching record exactly once across successful `--since last` invocations, with no re-emission after commit.
- Concurrent local exporters are serialized across cursor read, database snapshot, collection, render, merge, and commit; aggregate output contains each eligible record exactly once.
- Cursor readers and writers coordinate on a stable sidecar lock. Writers merge from the locked current state and cannot replace it with a regressed stale candidate.
- Pull-request/read-only `last` filtering acquires its shared cursor guard before loading the review database, retains it through filtering, and releases it immediately afterward; it never validates a new checkpoint against a stale snapshot, consumes the cursor, or holds the cursor lock during GitHub mutations/network calls. If a delivery-ledger lock is also held, acquisition order is always ledger then cursor.
- Cursor replacement uses a same-directory unique temporary file, complete write, temporary-file `sync_all`, atomic rename, and parent-directory `sync_all`.
- An injected failure before rename leaves the complete prior cursor unchanged; concurrent readers observe only complete old or complete new cursor states.
- Missing cursor still starts from all records, while empty, legacy timestamp-only, unversioned old JSON, unsupported-version, and invariant-invalid cursor files fail clearly instead of silently resetting progress.
- Focused cursor tests, the full `bug_regressions` target, and final `just check` pass.

## Non-goals and risks

- Preserving, auto-detecting, or migrating the legacy timestamp-only or unversioned timestamp/ID cursor formats is explicitly out of scope. This beta clean cutover should reject them clearly; users may deliberately remove the old cursor to restart `--since last` from all records.
- Changing the semantics of explicit `--since all`, unix/RFC3339/relative timestamp filters, or making those modes consume the `last` cursor is out of scope.
- Changing GitHub pull-request delivery-ledger behavior is out of scope. The PR path reads the new cursor through the shared-lock API using fixed ledger-then-cursor ordering, but releases the cursor guard immediately after filtering and does not merge the two persistence protocols.
- Rewriting, compacting, truncating, or reordering `reviews.jsonl` is out of scope. The cursor detects a logical-prefix mismatch and fails safe; automatic rebasing cannot distinguish already delivered records from replacement history without retaining unbounded delivery history.
- A permanently narrow filter can make `pending_record_ids` grow without bound. This is an inherent cost of promising later export under arbitrary changing selections; silently pruning those IDs is forbidden. The design avoids unbounded delivered-ID history.
- Holding the exclusive cursor lock through context resolution and output serializes local `--since last` commands and may make a second command wait. That is intentional for exactly-once successful output; other since modes and record appends remain independent.
- File state and stdout are not one atomic transaction. A process killed after output is externally consumed but before cursor rename may repeat that output on retry; committing earlier would instead risk permanent loss. The required guarantee is exactly once across commands that successfully render and commit, with at-least-once retry safety across abrupt external termination.
- Parent-directory syncing can be unsupported on some platforms. Propagate a focused durability error and preserve complete old/new cursor bytes; do not fall back to direct truncating writes.

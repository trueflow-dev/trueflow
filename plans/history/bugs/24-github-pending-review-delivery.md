# Issue #24: GitHub pending-review thread appends and exactly-once delivery

Status: ready
Date: 2026-07-10
Baseline commit: 9a98914698c4

## Problem

Pull-request feedback has two related delivery defects.

First, `GhGitHubClient::add_comment_to_pending_pull_request_review` sends line-oriented fields (`line`, `side`, `startLine`, and `startSide`) to the deprecated `addPullRequestReviewComment` mutation. Those fields are not part of `AddPullRequestReviewCommentInput`, so a real GitHub GraphQL schema rejects both single-line and multiline appends to an existing pending review. GitHub provides `addPullRequestReviewThread` for exactly this line/range form.

Second, `run_prepared_pull_request_feedback_with_filters` performs each remote create or append before the corresponding `GitHubDeliveryLedger` update is durably committed. The ledger is also loaded and overwritten without a repository-scoped lock, and `GitHubDeliveryLedger::save` uses a direct `fs::write`. A concurrent process, a crash, or a local save failure can therefore leave an accepted GitHub mutation absent from local state. A later run treats the same record as unstaged and can create a duplicate review comment.

The repair has two ordered slices:

1. use the schema-compatible review-thread mutation and validate the returned thread;
2. coordinate delivery through a repository lock and atomic, durable intent ledger, then reconcile uncertain operations from GitHub before doing anything that could repeat a remote side effect.

For this issue, the exactly-once safety contract is deliberately precise: a delivery unit identified by `(pull request, head SHA, record ID, operation ID)` produces at most one remote comment, and every mutation for which GitHub returned a validated acknowledgement is either in the atomic local ledger or recoverable by its durable marker from GitHub. An operation whose request may have been sent but whose result remains unobservable is not automatically repeated. This preserves at-most-once delivery rather than trading duplicates for liveness.

## Evidence

### Incompatible append mutation

- `trueflow/src/github.rs`, `GhGitHubClient::add_comment_to_pending_pull_request_review`, currently declares and invokes `addPullRequestReviewComment`, but supplies `line`, `side`, `startLine`, and `startSide` in its input.
- `ensure_add_pull_request_review_comment_response_success` correspondingly requires `/data/addPullRequestReviewComment/comment/id`, so response validation is coupled to the wrong mutation.
- The existing parser tests in `github.rs` validate the old `comment.id` path rather than the supported `thread.id` path.
- GitHub's official GraphQL pull-request reference says `AddPullRequestReviewCommentInput` uses legacy comment fields and marks those fields for replacement by the thread mutations; it does not define `line`, `side`, `startLine`, or `startSide` on that input: [GitHub `addPullRequestReviewComment` and `AddPullRequestReviewCommentInput`](https://docs.github.com/en/graphql/reference/pulls#addpullrequestreviewcomment).
- The same official reference says `addPullRequestReviewThread` “Adds a new thread to a pending Pull Request Review,” returns a `thread`, and accepts `AddPullRequestReviewThreadInput`: [GitHub `addPullRequestReviewThread`](https://docs.github.com/en/graphql/reference/pulls#addpullrequestreviewthread).
- `AddPullRequestReviewThreadInput` defines `pullRequestReviewId`, `body`, `path`, `line`, `side`, `startLine`, `startSide`, `subjectType`, and `clientMutationId`. It explicitly describes `line` as the end of a multiline range and `startLine` as the first line: [GitHub `AddPullRequestReviewThreadInput`](https://docs.github.com/en/graphql/reference/pulls#addpullrequestreviewthreadinput).
- The returned `PullRequestReviewThread` has a non-null node `id` and a `comments` connection, which are sufficient to validate and retain a receipt for the new thread: [GitHub `PullRequestReviewThread`](https://docs.github.com/en/graphql/reference/pulls#pullrequestreviewthread).

### Non-durable side-effect ordering

- `trueflow/src/commands/feedback.rs`, `run_prepared_pull_request_feedback_with_filters`, loads `github_delivery.json` before any repository lock exists.
- In the create branch, `create_pending_pull_request_review` runs before `record_pending_review` and before the final `ledger.save`. A successful POST followed by process termination or a save error leaves no local review ID or staged record IDs.
- In the append branch, each `add_comment_to_pending_pull_request_review` runs before `record_pending_review` and its per-comment `ledger.save`. An accepted thread followed by a save failure is eligible for the same append on retry.
- `parse_posted_pull_request_review` accepts any numeric `id` (including zero), any `html_url` string (including blank), any state, and optional/blank `node_id`/body. The create path therefore treats a structurally parsed but unusable or mismatched REST envelope as acknowledgement and can fold an invalid receipt instead of leaving the already-dispatched intent available for reconciliation.
- The existing `pull_request_feedback_records_successful_appends_before_later_append_failure` test covers an ordinary second-call failure only because the first ledger save succeeds. It does not cover the crash/save-failure window between a successful remote append and local persistence.
- `trueflow/src/github_delivery.rs`, `GitHubDeliveryLedger::load`, reads an unlocked path. `GitHubDeliveryLedger::save` serializes and directly calls `fs::write`, so another writer can overwrite newer state and a failed write can leave a partial ledger.
- `sync_pending_reviews` can inspect only review IDs already present in the ledger via `pull_request_review_status`. It cannot discover a successfully created but never-recorded review, nor can it discover an appended thread that succeeded after the last durable ledger version.
- The current missing-review branch drops the pending entry and makes its staged IDs eligible again. That is incompatible with the new at-most-once invariant for an already acknowledged mutation.
- `fs2` is already a normal dependency and `store.rs` already uses `fs2::FileExt` for file locking, so repository serialization does not require a new locking dependency. `uuid` is also already available for opaque operation identities.

### Remote reconciliation is supported by the GraphQL schema

- The official `PullRequest` object exposes `headRefOid`, `reviews`, and `reviewThreads`, including pagination: [GitHub `PullRequest`](https://docs.github.com/en/graphql/reference/pulls#pullrequest).
- `PullRequestReview` exposes `id`, `fullDatabaseId`, `body`, `commit`, `state`, `url`, `viewerDidAuthor`, and a `comments` connection. These fields can identify a trueflow-owned review and verify its head and author: [GitHub `PullRequestReview`](https://docs.github.com/en/graphql/reference/pulls#pullrequestreview).
- `PullRequestReviewComment` exposes `id`, `body`, `path`, `line`, `startLine`, `state`, `pullRequestReview`, and `viewerDidAuthor`. Together with the containing thread's sides and node ID, this permits exact operation-marker and payload reconciliation rather than fuzzy body matching: [GitHub `PullRequestReviewComment`](https://docs.github.com/en/graphql/reference/pulls#pullrequestreviewcomment).
- GitHub describes `clientMutationId` only as a client-provided unique identifier returned by the mutation. The documentation does not promise server-side idempotent deduplication, so it must not be treated as a retry key.

## Reproduction

### Append schema failure

1. Prepare a repository with a current-head trueflow pending review in `github_delivery.json` and one new line-anchored feedback record.
2. Put a fake `gh` executable first on the child process's `PATH`. Have it capture the JSON sent to `gh api --method POST graphql --input -`, validate the GraphQL operation and input against GitHub's documented pull-request schema, and return a GraphQL `errors` response for unknown input fields.
3. Run `trueflow feedback --pr pr:11`.
4. At the baseline, the captured operation is `addPullRequestReviewComment` and contains `line`/`side`; the fake schema rejects it. Repeat with a multiline record and observe that `startLine`/`startSide` are also sent to the incompatible input.

### Lost create acknowledgement

1. Stage feedback with no reusable current-head pending review.
2. Let the fake GitHub remote accept the create-review POST and retain the review body/comments, then force the following local ledger save to fail.
3. Restore filesystem writability and run the same command again.
4. At the baseline, no durable operation intent or review ID exists, `sync_pending_reviews` has nothing to query, and the second run creates another pending review containing the same comments.

### Concurrent append

1. Start two `trueflow feedback --pr pr:11` processes against the same repository and staged record.
2. Block the first fake-GitHub append after both processes have had time to load the baseline ledger.
3. Release the first append and let both processes finish.
4. At the baseline, both can select and append the same record, and their direct whole-file ledger writes race. There is no lock spanning load, delivery selection, remote mutation, and commit.

## Root cause

The schema bug is a direct mutation/input mismatch. `GitHubInlineComment` already contains file-line coordinates, but the client sends them through the legacy comment mutation rather than the line/range thread mutation that owns those fields. The old response parser masks the mismatch in unit tests because it is tested only with hand-authored JSON and never against a schema-aware `gh` boundary.

The duplicate-delivery bug is a distributed commit-ordering problem, not merely a missing `fs::write` retry:

1. remote side effects have no durable local operation identity before dispatch;
2. the operation identity is not embedded in the remote review/comment body, so an orphaned success cannot be found later;
3. `GitHubDeliveryLedger` has no explicit prepared/in-flight operation state;
4. load/modify/save is not serialized across processes;
5. save does not atomically replace a previously valid ledger;
6. reconciliation only follows ledger-known numeric review IDs and cannot enumerate trueflow-owned reviews/threads;
7. an unknown request outcome is currently indistinguishable from a safe request to repeat.

Atomic local persistence and a lock close the local concurrency window, but they cannot atomically commit GitHub and the filesystem. The remaining crash boundary must therefore be closed with a write-ahead intent and a remote identity marker. Once an intent is durably marked in-flight, a subsequent process must reconcile it against a complete remote snapshot. It may adopt an exact match, but it must not blindly reissue an absent or ambiguous in-flight operation.

## Implementation plan

### Slice 1: use `addPullRequestReviewThread`

1. **Add red, schema-aware fake-`gh` tests before changing the client.**
   - Create `trueflow/tests/github_pending_review_delivery.rs` with a child-process fake-`gh` harness. The executable must capture argv and stdin per call and return scripted JSON; the Rust side must parse every captured request with `serde_json` and reject any operation/input shape outside the documented schema instead of matching source text.
   - Add `gh_pending_review_append_uses_thread_for_single_line`. Seed a trueflow-owned pending review and one single-line record, run the real `trueflow feedback --pr pr:11` child with the fake on `PATH`, and assert the GraphQL request uses `addPullRequestReviewThread(input: $input)`, `AddPullRequestReviewThreadInput!`, and `pullRequestReviewId`, `body`, `path`, `line`, `side`, `subjectType: LINE`. Assert `startLine` and `startSide` are omitted for a single-line comment.
   - Add `gh_pending_review_append_uses_thread_for_multiline`. Assert `line`/`side` are the range end and `startLine`/`startSide` are the range start, with both start fields present as a pair. Cover both `LEFT` and `RIGHT` serialization through the existing enum rather than string concatenation.
   - Have the fake return a success envelope containing `data.addPullRequestReviewThread.clientMutationId` and `thread.id`. Also add malformed envelopes: GraphQL `errors`, missing `data`, null/missing `thread`, blank thread ID, and a mismatched operation identity. These cases must remain errors because the local caller cannot safely acknowledge a thread it cannot identify.
   - The two schema tests must fail at the baseline by observing `addPullRequestReviewComment` and its incompatible input.

2. **Make the minimal client mutation and response-model change in `trueflow/src/github.rs`.**
   - Add a serializable `AddPullRequestReviewThreadInput` composition and an explicit `PullRequestReviewThreadSubjectType::Line` enum that serializes as `LINE`; use `skip_serializing_if = "Option::is_none"` for paired start fields.
   - Validate the `GitHubInlineComment` range invariant before spawning `gh`: `start_line` and `start_side` are either both absent or both present, and a present start is not after the end line. Do not send half-ranges.
   - Replace the legacy mutation with a single `$input: AddPullRequestReviewThreadInput!` variable and request `addPullRequestReviewThread { clientMutationId thread { id } }`. Do not depend on an undocumented ordering of the thread's `comments` connection merely to validate this mutation.
   - Replace `ensure_add_pull_request_review_comment_response_success` with typed thread-response parsing. Reuse the existing GraphQL error aggregation, then require the requested operation identity to be echoed and a non-blank `thread.id`. Return a small `PostedPullRequestReviewThread` receipt instead of `Result<()>`; this receipt is consumed by slice 2.
   - Remove the old parser and old `comment.id` tests. Do not leave an alias or compatibility path for `addPullRequestReviewComment`.

3. **Run the slice-1 smoke before starting durability work.**
   - Run the two exact fake-`gh` append tests listed under verification.
   - Confirm the fake rejects the legacy operation, a single-line input has no nullable range fields, a multiline input has both range-start fields, and malformed success envelopes are not accepted.

### Slice 2: serialize, journal, and reconcile delivery

4. **Add red end-to-end failure-boundary tests to `trueflow/tests/github_pending_review_delivery.rs`.**
   - Build one reusable fake-`gh` remote model that is schema-aware for the REST review endpoints and GraphQL review/thread operations. It must record remote reviews and threads separately from the local ledger, support deterministic gates/failures, return `pageInfo`, and let the Rust test assert exact mutation counts and operation-marker uniqueness.
   - Use a local bare Git remote plus Git `url.*.insteadOf` in the fixture so the repository retains a GitHub-shaped `origin` for `prepare_pull_request_review` while `git fetch` stays local. The fake must return metadata and commit JSON whose SHAs match that fixture; do not bypass `prepare_pull_request_review` or `run_prepared_pull_request_feedback_with_filters`.
   - Add one narrow crash-boundary handshake compiled only by the existing `tui-test-support` feature. When `TRUEFLOW_TEST_GITHUB_DELIVERY_CHECKPOINT_DIR` is set, the delivery path writes a ready file containing the operation ID immediately after the atomic `Prepared` save, before attempting the `InFlight` save, and blocks on a release file. The integration test waits for that ready file, asserts the persisted state and zero fake-`gh` mutations, then kills the child. With the environment unset—or in builds without the feature—the checkpoint is a no-op; do not use polling/chmod races or a production CLI flag.
   - Add `github_feedback_serializes_concurrent_workers`. Spawn two real trueflow child processes against the same repository. Gate the first remote mutation after its in-flight intent is durably visible, start the second, and prove it cannot reach any create/append mutation until the first releases the repository lock. After both finish, assert one remote comment for the staged record, one accepted ledger entry, and no lost ledger state.
   - Add `github_feedback_recovers_created_review_after_ledger_save_failure`. Let create succeed remotely, then deterministically fail the first post-acknowledgement ledger replacement while preserving the pre-dispatch in-flight ledger. On the next process, script paginated review/thread discovery to return the remote operation markers. Assert the retry performs no second create or append, adopts the original review ID/node ID/URL and every comment operation into local state, and excludes the staged records.
   - Add parameterized `github_feedback_keeps_malformed_create_receipt_in_flight`. Have the fake remote accept and store the exact marked create request but return envelopes with, in turn, non-`PENDING` state, zero database ID, blank/missing node ID, blank URL, missing/wrong create-operation marker or head, and a mismatched `commit_id` when that field is present. Each first run must fail with the durable intent still `InFlight` and no accepted local receipt. A subsequent complete discovery of the stored review/comments must recover it without a second create.
   - Add `github_feedback_recovers_appended_thread_after_ledger_save_failure`. Let an append return a valid thread receipt, fail its local acknowledgement save, and rerun. Make the reconciliation fixture return multiple pages from that thread's `comments` connection, with an unrelated reply on an earlier page and the marked root comment on a later page. Assert reconciliation follows every cursor, identifies the root by `replyTo == null` rather than connection position, finds the exact marker/thread ID, updates the existing pending review, and leaves the remote append count at one.
   - Add `github_feedback_resumes_prepared_intent_after_restart`. Parameterize the fixture over create and append intents. Start the real feature-enabled child with the checkpoint directory, wait for the deterministic post-`Prepared`/pre-`InFlight` ready handshake, assert the fake received no mutation, kill the blocked child, and restart without the checkpoint environment. Assert the second worker reuses the exact persisted operation/comment IDs and marked payload, atomically advances that same intent to `InFlight`, sends it once, and folds its receipt; it must not exclude-and-strand the prepared work or synthesize a replacement intent.
   - Add `github_feedback_replans_prepared_append_when_target_closes`. Stop after persisting a current-head `Prepared::AppendReviewThread`, then have the fake remote submit or delete that target review before restart. Assert the restarted worker revalidates the still-unsent destination while the intent is `Prepared`, atomically cancels it, replans the same record against the now-current delivery destination, and sends once there. A lookup/pagination failure must instead leave the original intent prepared and send nothing.
   - Add `github_feedback_preserves_accepted_prefix_after_append_failure`. Stage two records, accept the first thread, and make the second request fail after it is marked in-flight. Assert the first receipt remains accepted/excluded, the second is not falsely acknowledged, a later run does not repeat either ambiguous in-flight request, and the remote contains one copy of the accepted first comment. The unresolved second intent must produce an actionable error rather than disappearing or being treated as delivered.
   - Add `github_feedback_keeps_stale_head_delivery_separate`. Seed a trueflow pending review and operation markers for an older head, then run against the current head. Assert stale review IDs and markers are neither appended to nor adopted as current-head receipts; a current-head operation has a distinct identity, and reconciliation requires the intended head/commit as well as the marker. Preserve the current behavior that stale-head staged IDs do not suppress a current-head delivery.
   - Add `github_feedback_releases_cursor_guard_before_remote_mutation` against the plan-22 cursor API. Run PR feedback with `--since last`, gate the fake remote mutation while the GitHub ledger lock is still held, and assert a separate process can acquire the exclusive `feedback.cursor.lock`. This proves the shared `FeedbackCursorReadGuard` covered the database snapshot/filter but was dropped before GitHub I/O.
   - For every scenario, parse the captured GraphQL requests in Rust and enforce the documented field names/types, pagination variables, response paths, and `clientMutationId`; a call-count-only fake is insufficient.

5. **Replace the v1 ledger with an explicit v2 write-ahead model in `trueflow/src/github_delivery.rs`.**
   - Bump `GITHUB_DELIVERY_LEDGER_VERSION` to 2 and use a clean schema cutover. Because the project is beta, do not add v1 deserialization defaults, migration logic, aliases, or a silent reset. Reject a non-v2 ledger with an error that preserves the file and explains that delivery cannot continue safely until remote state is resolved.
   - Compose the schema from explicit structs/enums instead of parallel optional fields:
     - an opaque `GitHubDeliveryOperationId` generated once and persisted;
     - `GitHubDeliveryIntent::{CreatePendingReview, AppendReviewThread}` with the PR, intended head SHA, exact marked request payload, and one `(record_id, comment_operation_id)` entry per comment;
     - `GitHubDeliveryIntentStatus::{Prepared, InFlight}`;
     - accepted comment receipts containing record ID, operation ID, and available review/thread/comment node IDs;
     - pending-review state containing the review database ID, review node ID, URL, head SHA, create operation ID when applicable, and accepted comment receipts;
     - terminal/tombstoned receipts that remain excluded after submission, deletion, or disappearance.
   - Store the exact marked body and anchor payload in each intent. Before persisting an intent, enforce that each `(PR, head SHA, record ID)` appears in at most one active or accepted comment entry and that a create batch contains no duplicate record IDs.
   - Include `Prepared` and `InFlight` record IDs in head-scoped exclusion. A process restart must reuse a durable intent and operation ID, never manufacture a second operation for the same staged unit.
   - Define the transition invariant in code and tests: `Prepared` is durable before any remote call; `InFlight` is atomically durable before bytes may be sent to `gh`; validated remote success is folded into accepted pending/terminal state by one atomic save. Any failure after dispatch leaves `InFlight` unless exact remote reconciliation proves acceptance.
   - Permit cancellation/replanning only from `Prepared`, where the write-ahead invariant proves no request bytes were sent. The cancellation transition must be atomic and make those record IDs eligible again; there is no corresponding `InFlight -> Prepared` or `InFlight -> cancelled` shortcut.
   - Change missing-review synchronization so accepted receipts become terminal/tombstoned and remain excluded rather than being forgotten and redelivered. Terminal GitHub review states move the same receipts to completed state without losing operation identity.

6. **Put locking and atomic replacement behind one ledger-store API in `trueflow/src/github_delivery.rs`.**
   - Add a `GitHubDeliveryLedgerStore`/locked-session struct for the `.trueflow` directory. Open a stable `.trueflow/github_delivery.lock` file and acquire `fs2::FileExt::lock_exclusive` before loading the ledger. Do not lock `github_delivery.json` itself because atomic rename changes that inode.
   - Hold the guard across ledger load, pending-review sync, in-flight reconciliation, record filtering/planning, delivery selection, every create/append mutation, and the final ledger commit. The submission path must use the same lock so submit cannot race an append, although submission idempotency is not expanded in this issue.
   - Preserve one cross-plan nested-lock order: acquire the GitHub ledger lock first, then (only while taking a plan-22 `--since last` snapshot) acquire `FeedbackCursorReadGuard`. No path may acquire the ledger lock while already holding either the shared read guard or `FeedbackCursorUpdateGuard`. Release the cursor guard immediately after record filtering while retaining the ledger lock for planning, intent transitions, remote mutations, and ledger commit.
   - Replace direct `fs::write` with same-directory atomic persistence: serialize fully, create a uniquely named file with `create_new`, write all bytes, `flush`, `sync_all` the file, atomically rename it over `github_delivery.json`, and sync the parent directory where supported. On any pre-rename failure, leave the last valid ledger untouched; never parse a corrupt ledger as empty.
   - Keep the lock file separate and persistent. Release it by dropping the guard only after the delivery state is durable; open-browser behavior occurs after release.
   - Reuse the existing `fs2` and `uuid` dependencies. Do not add a second lock implementation or a general transaction framework.

7. **Add durable remote identities and discovery to `trueflow/src/github.rs`.**
   - Add hidden, versioned operation markers to the data sent remotely. The pending review body carries its create operation ID and head SHA in addition to `TRUEFLOW_PENDING_REVIEW_MARKER`; every first comment body carries its own operation ID. Add markers only when materializing a delivery intent so dry-run/user-facing plan text remains unchanged.
   - Set the append mutation's `clientMutationId` to the same operation ID, but use the body marker—not `clientMutationId`—as the durable reconciliation key because GitHub does not document deduplication.
   - Extend `GitHubClient` with a delivery-snapshot/reconciliation method represented by explicit review, thread, and comment-page structs. The `GhGitHubClient` implementation must page through `PullRequest.reviews` and `PullRequest.reviewThreads` until each `pageInfo.hasNextPage` is false, then page `PullRequestReviewThread.comments` separately for every candidate thread. Nested connections cannot share one cursor, and absence must never be inferred from `comments(first: 1)` or any partial review, thread, or comment page.
   - Query the PR `headRefOid`; for reviews query node ID, `fullDatabaseId`, URL, body, state, `viewerDidAuthor`, and `commit.oid`; for threads query node ID, path, and end/start lines and sides. For every thread-comment page query comment ID, body, `viewerDidAuthor`, state, parent review ID, `replyTo { id }`, and `pageInfo`. Parse all envelopes and cursors with typed response structs and the existing GraphQL error handling.
   - Search all returned comments for the operation marker and require the matching comment to be the root (`replyTo == null`); do not assume GitHub returns the initial comment first because the documented connection has no `orderBy` contract. Reconciliation accepts exactly one root marker only when PR, head/commit, current-viewer authorship, review ownership marker, operation type, visible body, path, line/range, and sides match the durable intent. Zero matches after complete pagination leaves an in-flight operation unresolved; multiple matches or a payload mismatch is an invariant error. Neither case sends another mutation.
   - If a reconciled review is still pending, rebuild pending state. If it is terminal, rebuild completed state. This allows a create/append accepted before a local save failure to become local state even if a human submits the review before the next trueflow run.
   - Split strict create acknowledgement from the permissive status parser. Add a typed `parse_created_pending_pull_request_review` used only by `create_pending_pull_request_review`; require `PENDING`, database ID greater than zero, non-blank node ID and URL, and the exact durable create-operation marker/head in the returned body. Deserialize `commit_id` and require it to equal the intended head whenever GitHub includes it. Keep `parse_posted_pull_request_review` for status lookup only; do not let its optional fields authorize an `InFlight -> accepted` transition.

8. **Reorder orchestration in `trueflow/src/commands/feedback.rs`.**
   - Acquire the locked ledger session at the start of `run_prepared_pull_request_feedback_with_filters`, before the current `load`/`sync` sequence and before plan 22's `load_pull_request_feedback_snapshot_with` can acquire `FeedbackCursorReadGuard`. This fixes the only nested order as ledger lock → cursor read guard.
   - Reconcile all in-flight intents for the target PR before selecting records or a destination. Exact remote matches are folded and atomically saved first. An unresolved or conflicting in-flight intent returns an error without planning or sending new mutations.
   - Drop stale `Prepared` intents that are provably never dispatched when their intended head is no longer current. Reconcile stale `InFlight` intents only against their recorded head; never attach them to a current-head review.
   - Resume surviving current-head `Prepared` intents before planning new records, but first revalidate their persisted destination from a complete remote snapshot while they are still provably unsent. For append, require the exact target review to exist, remain `PENDING`, remain trueflow-owned/current-viewer-authored, and match the intended head; for create, require the intended head still to be current and rerun destination selection so a newly appeared reusable pending review is not ignored. If revalidation is incomplete, leave the intent `Prepared` and abort without mutation. If it definitively shows a stale/terminal/deleted/ineligible destination, atomically cancel only that `Prepared` intent so its exact records can be replanned in the same locked run. Otherwise reuse its persisted operation IDs, markers, and exact payload, atomically transition that same intent to `InFlight`, and dispatch it. Record exclusions prevent duplicate planning; they must not strand a valid prepared intent.
   - Build the feedback plan from accepted plus active head-scoped record exclusions. Materialize markers and persist all new `Prepared` intent payloads before delivery.
   - For new planning under effective `--since last`, call the plan-22 snapshot helper while the ledger guard is held, retain `FeedbackCursorReadGuard` through database load and `filter_pull_request_feedback_records`, materialize the filtered `Vec<Record>`, then drop the snapshot/read guard immediately. Continue planning, intent persistence, and all GitHub calls with only the ledger lock held; no cursor lock may span remote reconciliation or mutation.
   - Immediately before each remote create/append, atomically transition that intent to `InFlight`. On validated success, use the returned review/thread receipt to atomically fold it into accepted state. Do not call the old `record_pending_review`-then-`save` sequence after an unjournaled remote mutation.
   - Invoke the feature-gated checkpoint only after a successful atomic `Prepared` commit returns and immediately before the `InFlight` transition. Keep the hook private to orchestration/test support and pass only the immutable operation ID; it must not alter ledger state, select behavior, or exist in normal release builds.
   - For create, keep the existing REST review creation API, but send the marked review body and marked comment bodies from the durable create intent. For append, pass the durable operation ID/marked body to the new thread mutation and retain its thread receipt.
   - Treat spawn failures known to occur before a child exists as safe pre-dispatch failures only if the implementation can prove no request bytes were sent. Treat transport errors, malformed success responses, GraphQL partial responses, process termination, and post-ack save failures as uncertain `InFlight` outcomes requiring reconciliation. Do not add an automatic remote retry loop.
   - A create call may fold the intent only after the strict create parser validates the response against that intent. A malformed or mismatched 2xx envelope is an uncertain post-dispatch result: return the parsing error with the intent still `InFlight`, then recover through remote discovery on a later run rather than issuing another create.
   - Preserve current filtering, dry-run reporting, open-URL behavior, pending-review ownership checks, and current-head selection. Dry-run may compute a destination but must not create intents, markers, lock-independent writes, or remote mutations.

9. **Update focused unit coverage after the behavior works.**
   - In `github.rs`, replace old response-path tests with typed thread receipt tests, add strict create-receipt parser cases for every required/mismatched field, and add pagination/marker parsing tests for remote discovery.
   - In `github_delivery.rs`, test strict v2 version checking, duplicate record/operation rejection, `Prepared -> InFlight -> accepted` transitions, active-intent exclusions, terminal/tombstone retention, atomic-save preservation of the previous valid document on failure, and lock serialization with separate file handles.
   - In `commands/feedback.rs`, adapt `FeedbackTestGitHubClient` to return thread receipts and delivery snapshots. Keep fast in-process tests for selection/filter behavior, but rely on the fake-`gh` integration target for command shape, process interleaving, and crash/save boundaries.
   - Remove the old direct `GitHubDeliveryLedger::load`/`save` call pattern from production call sites and remove obsolete v1-only test fixtures. Do not retain a bypass around the locked store.

10. **Run focused smoke, then cleanup and full validation.**
    - Run the complete fake-`gh` integration target and the focused ledger/client/feedback unit modules.
    - Inspect the fake remote model and final v2 ledger for every scenario: remote marker uniqueness, exact mutation counts, operation-state transitions, and record exclusions must agree.
    - Only after those focused tests pass, remove temporary failpoints/gates that are not contained in test fixtures, run formatting through the repository's normal final gate, and run `just check`.

## Verification and validation

Run from the repository root in this order.

### Slice-1 red/green checks

```sh
cd trueflow && cargo test --test github_pending_review_delivery gh_pending_review_append_uses_thread_for_single_line -- --exact
cd trueflow && cargo test --test github_pending_review_delivery gh_pending_review_append_uses_thread_for_multiline -- --exact
cd trueflow && cargo test --lib github::tests::add_review_thread
```

Behavioral checks:

- captured GraphQL contains `addPullRequestReviewThread`, never `addPullRequestReviewComment`;
- the input variable is an object of the documented thread-input shape;
- single-line appends omit both range-start fields;
- multiline appends send both range-start fields and preserve side enums;
- a response is successful only with the expected mutation payload, echoed operation identity, and non-blank thread ID;
- GraphQL errors or malformed/partial data return an error and do not acknowledge the operation.

### Slice-2 focused checks

```sh
cd trueflow && cargo test --lib github_delivery::tests
cd trueflow && cargo test --lib commands::feedback::tests::pull_request_feedback_
cd trueflow && cargo test --test github_pending_review_delivery github_feedback_serializes_concurrent_workers -- --exact
cd trueflow && cargo test --test github_pending_review_delivery github_feedback_releases_cursor_guard_before_remote_mutation -- --exact
cd trueflow && cargo test --test github_pending_review_delivery github_feedback_recovers_created_review_after_ledger_save_failure -- --exact
cd trueflow && cargo test --test github_pending_review_delivery github_feedback_keeps_malformed_create_receipt_in_flight -- --exact
cd trueflow && cargo test --test github_pending_review_delivery github_feedback_recovers_appended_thread_after_ledger_save_failure -- --exact
cd trueflow && cargo test --features tui-test-support --test github_pending_review_delivery github_feedback_resumes_prepared_intent_after_restart -- --exact
cd trueflow && cargo test --test github_pending_review_delivery github_feedback_preserves_accepted_prefix_after_append_failure -- --exact
cd trueflow && cargo test --test github_pending_review_delivery github_feedback_replans_prepared_append_when_target_closes -- --exact
cd trueflow && cargo test --test github_pending_review_delivery github_feedback_keeps_stale_head_delivery_separate -- --exact
cd trueflow && cargo test --features tui-test-support --test github_pending_review_delivery
```

Behavioral checks:

- while worker one is gated inside its remote mutation, worker two cannot enter a create/append mutation; after release there is one remote comment and one accepted receipt for the staged delivery unit;
- the nested lock order is GitHub ledger lock then `FeedbackCursorReadGuard`; once filtering completes, a cursor writer can acquire `feedback.cursor.lock` even while the GitHub worker remains gated in a remote mutation;
- after create success plus ledger-save failure, the durable ledger remains readable, the next process discovers the marked review and all marked comments, and the remote create count stays one;
- a malformed or mismatched create-review 2xx response never becomes accepted local state; the intent stays `InFlight`, and later discovery recovers the one stored remote review without another create;
- after append success plus ledger-save failure, the next process pages every candidate thread's comments, locates the marked root comment independently of connection order, records the thread/comment receipt, and leaves the remote append count at one;
- the feature-gated ready-file handshake proves the killed worker is blocked after the complete atomic `Prepared` save and before `InFlight` or any fake-`gh` call; restart reuses and dispatches that original create/append intent exactly once, with no timing race, stranded exclusion, or replacement operation ID;
- after a later append fails, earlier accepted receipts remain excluded and are not repeated; the failed/ambiguous intent remains explicit rather than being dropped or falsely accepted;
- after a persisted but unsent append target is submitted or deleted, restart cancels only the `Prepared` intent and replans its record to a valid current destination; an incomplete lookup leaves it prepared and performs no mutation;
- a stale-head review is never reused or reconciled as a current-head destination, and operation identity is head-scoped;
- a partial reviews, review-threads, or per-thread comments pagination response is never treated as proof that an operation is absent;
- killing a worker after the in-flight save but before local acknowledgement leaves valid JSON and produces reconciliation, not a second mutation, on the next invocation;
- replacing or truncating a ledger never silently yields an empty default ledger;
- no production path invokes a remote create/append before its exact intent and identity are durably in-flight.

### Final repository gate

```sh
just check
```

## Acceptance criteria

- Single-line and multiline pending-review appends use `addPullRequestReviewThread` with the documented input fields and `LINE` subject type.
- `GhGitHubClient` validates GraphQL errors, operation identity, and the returned thread ID; it never accepts the old `comment.id` response shape or relies on undocumented thread-comment ordering.
- Create acknowledgements are accepted only for a `PENDING` review with nonzero database ID, nonblank node ID/URL, the exact durable create marker/head, and a matching `commit_id` when present; malformed/mismatched responses remain `InFlight`.
- A stable repository lock serializes the complete ledger read/reconcile/plan/mutate/commit critical section across processes.
- When issue 22's cursor transaction is present, PR delivery observes the fixed ledger-lock → cursor-read-guard order and drops the cursor guard after filtering, so no cursor lock spans GitHub reconciliation or mutation.
- Ledger replacement is atomic and durable: a failed save cannot expose a truncated document or erase the last valid state.
- Every create/append has a durable operation ID and exact intent before dispatch, and every remote review/comment carries a hidden reconciliation marker for that identity.
- Active intents participate in record exclusion, so a restart cannot stage a second operation for the same `(PR, head SHA, record ID)`.
- Remote discovery is complete and paginated, verifies author/ownership/head/payload, and adopts exactly one matching accepted create/append rather than repeating it.
- An absent, conflicting, duplicated, malformed, or otherwise ambiguous in-flight remote result blocks automatic redelivery and remains actionable local state.
- The schema-aware fake-`gh` tests cover single-line append, multiline append, concurrent worker interleaving, restart from a durable current-head `Prepared` intent, safe replanning when a prepared append target closes, strict/malformed create receipts, create-success/save-failure retry, append-success/save-failure reconciliation, append partial failure, and stale-head behavior.
- Existing dry-run, filtering, submit coordination, current-head selection, and open-URL behavior remain intact.
- Focused tests and `just check` pass.

## Non-goals and risks

- **Cross-plan dependency:** apply the validated bug plans in order 21 → 22 → 23 → 24. This plan consumes issue 22's `FeedbackCursorReadGuard`/snapshot helper and must preserve its database-snapshot coverage while adding the outer GitHub ledger lock.
- **No blind retry/backoff loop.** This issue does not add retries for create or append requests whose remote outcome could be accepted; such retries can duplicate side effects. A durable `InFlight` operation that is not found by a complete reconciliation remains unresolved and blocks, favoring at-most-once safety over automatic progress.
- **No claim that `clientMutationId` is server idempotency.** It is sent and validated as correlation metadata, while the durable hidden marker and remote query provide recovery.
- **No ledger migration or compatibility shim.** The v2 schema is a beta clean cutover. A v1, corrupt, or unknown-version ledger must fail closed rather than silently reset and risk reposting feedback.
- **No change to feedback selection or line mapping.** Record filters, rename mapping, diff translation, and comment content are unchanged except for hidden delivery markers added at dispatch materialization.
- **No replacement of the create-review API solely for symmetry.** The existing REST create endpoint may remain; exactly-once recovery comes from the durable create intent, markers, validated create receipt, and GraphQL discovery.
- **No general-purpose distributed transaction framework.** The lock/store/intent types remain specific to GitHub delivery and use existing dependencies.
- **No duplicate cleanup on GitHub.** If duplicates created by an older version already exist, reconciliation reports the invariant violation and does not delete or guess which human-visible comment to keep.
- The filesystem lock coordinates cooperating trueflow processes in one repository; it cannot serialize unrelated clients that directly mutate GitHub.
- Holding the repository lock during network calls intentionally serializes delivery and can make a second worker wait. It is required to prevent interleaving; error messages should identify the repository operation if acquisition or persistence fails.
- Hidden markers can be edited or removed by a user. An in-flight operation that can no longer be matched must fail closed rather than repost. Exact payload and `viewerDidAuthor` checks prevent a copied/forged marker from being adopted silently.
- GitHub schema pagination or availability failures must abort reconciliation before mutation. Absence is trustworthy only after all required pages have parsed successfully.
- Head changes are a semantic boundary. The at-most-once unit is head-scoped so a stale pending review cannot suppress or absorb feedback intended for the current head.

# Issue #20: Generated review-unit coverage ambiguity

Status: ready  
Date: 2026-07-10  
Baseline commit: 9a98914698c4
Prerequisites: Issue #18 (`plans/bugs/18-block-byte-span-identity.md`) and Issue #19 (`plans/bugs/19-small-file-optimizer-source-integrity.md`)

## Problem

`CoverageIndex` enforces candidate cardinality for blocks represented by tree nodes, but not for review units produced later by `sub_splitter`. A generated block is absent from the tree, so `CoverageIndex::block` scans review records afresh for that individual block. If two generated units have the same content hash, the same coarse block record can therefore be treated as a direct approval for both units even though it does not identify either one uniquely.

This affects both coarse block-record forms:

- a hash-only record (`path_hint == None`), when the hash occurs in more than one generated unit in the coverage universe;
- a path-scoped record (`path_hint == Some(path)` and `line_hint == None`), when the path and hash identify more than one generated unit in that file.

The required invariant is:

> One coarse record can bind to at most one logical coverage unit. It must not clear multiple units when its candidate set is ambiguous.

Issues #18 and #19 are prerequisites, not parallel work. Issue #18 adds required absolute half-open byte spans to source-backed `Block` values and makes path+hash+`ByteSpan` the complete in-memory occurrence identity. Issue #19 makes optimized block content an exact source slice, so generated child byte spans can be translated from a truthful parent. Implementing this issue on the baseline line-only/fabricated-content model would recreate incomplete identity inside coverage.

Persisted review records intentionally remain path/hash/line based. An exact block record containing path, hash, and start line remains valid when that persisted tuple identifies one candidate, including duplicate hashes on distinct lines. If byte-distinct equal-hash units begin on the same line, the record schema cannot distinguish them; even an “exact” path+hash+line record must remain ambiguous rather than selecting one byte occurrence. This is not a rejection of exact location approvals—the distinct-line exact case remains a required control.

## Evidence

- `trueflow/src/coverage.rs:84-153` builds `CoverageIndex` by constructing `TreeCoverageLookups` and binding every database record once. Successful tree bindings populate direct and linked facts; ambiguous tree bindings populate only linked facts plus `CoverageDiagnostic::AmbiguousRecord`.
- `trueflow/src/coverage.rs:173-199` resolves `CoverageIndex::block` through `Tree::find_block_node`. When the block has no tree node, it bypasses those global bindings and calls `matching_record_indices_for_block`; every returned record is installed as both direct and linked coverage for that one query.
- `trueflow/src/coverage.rs:227-248` implements that fallback by scanning the full database independently for each generated block. It does not compare the current block with any other generated candidate before accepting the record.
- `trueflow/src/coverage.rs:638-695` builds exact, path+hash, and hash-only lookup tables solely from `TreeNodeKind::Block` nodes. Generated review units are not members of any lookup.
- `trueflow/src/coverage.rs:721-787` already applies the correct cardinality rule to tree blocks: exact, path-scoped, and hash-only candidate sets bind only when exactly one node remains; multiple candidates produce `AmbiguousRecord` and no direct binding.
- `trueflow/src/coverage.rs:933-957` is weaker than the tree binder. `record_match_relation_for_block` returns `HashOnly` for any queried block with the same hash and returns `PathScoped` for any queried block with the same path and hash. It has no knowledge of other matching generated units.
- `trueflow/src/coverage.rs:478-494` re-runs `record_match_relation_for_block` while choosing the latest verdict for a non-tree `BlockCoverage`, so the local per-block decision is repeated instead of consuming the binding computed by the index.
- `trueflow/src/commands/review.rs:999-1051` calls `coverage.block(file_path, sub_block).direct_latest_verdict_for(...)` for every generated `SubSplitSemantics::ReviewUnits` block. With generated units `S1 = (README.md, line 10, H)` and `S2 = (README.md, line 30, H)`, one approved hash-only record for `H` is accepted independently by both calls, allowing `is_subblock_covered` to clear the parent.
- `trueflow/src/commands/inspect.rs:84-97` similarly builds an index from the tree and then asks for coverage separately for each `inspect --split --coverage` sub-block. `build_coverage_summary` at lines 176-280 derives direct records, binding relation, and diagnostics from that result. Today a generated record can be reported as direct coverage by the fallback while `binding_relation_for_record` is `None` and the build-time diagnostic is `UnresolvedRecord`, because the generated candidate was never present during binding.
- `trueflow/src/coverage.rs:1214-1245` already proves the intended behavior for tree nodes: one hash-only record linked to two same-hash tree blocks is diagnosed as ambiguous, has no binding relation, and directly approves neither block. Generated review units must obey the same rule rather than form a second coverage convention.
- At the baseline, `trueflow/src/tree.rs:148-155` and coverage exact lookup use path, hash, and start line. Issue #18 demonstrates why that tuple is not a complete in-memory identity: equal-content units can occupy different byte ranges on one line. This issue must consume issue #18's complete `ByteSpan` identity, not reproduce the old line-only key in `CoverageBlockLocator`.
- `trueflow/src/block.rs:320-343` already defines `ByteSpan`, while issue #18 promotes it to a required, hashable, source-backed `Block` field with production containment and constructors. Issue #19 ensures optimized parent content and its span describe the same source slice before coverage derives generated children.
- `trueflow/src/sub_splitter.rs:18-35` explicitly distinguishes `ReviewUnits` from `StructuralChildren`. The candidate extension must cover generated `ReviewUnits` without changing structural-child coverage semantics.
- `trueflow/src/sub_splitter.rs:744-795` splits an over-limit Markdown paragraph into sentence review units. After issue #18, two identical sentences can share a content hash and line while retaining different absolute byte spans; `Twin. Twin. Tail.` yields two identical `Twin. ` units on one physical line.
- `trueflow/src/commands/review.rs:597-679` currently applies `allows_block`, whitespace-default, and import-default filters inside `collect_diff_review_files`. `collect_diff_scoped_review` then builds the diff tree and `CoverageIndex` at lines 460-470 from that already-filtered collection. A hidden changed block and its generated units therefore disappear before candidate cardinality is computed.
- The workdir path does not have that ordering defect: `collect_review` builds the tree and coverage at lines 334-336 before applying `allows_block` and default display skips in the block loop at lines 360-376. Diff review must follow the same bind-before-display-filter invariant.

## Reproduction

### Distinct-line Markdown regression

Add a regression fixture in `trueflow/tests/sub_block_coverage.rs` with one Markdown paragraph longer than 50 lines. Give every line a terminating sentence so `inspect --split` produces generated `Sentence` review units. Make two non-adjacent lines byte-identical, including their trailing newline, and keep the other sentence contents unique. Assert before recording reviews that:

1. the scanned parent is a tree block;
2. `inspect --fingerprint <parent-hash> --split` returns more than one generated unit;
3. exactly two generated units have the duplicate hash;
4. their start lines and complete byte spans differ; and
5. at least one other generated unit has a globally unique hash.

Record approved hash-only reviews for all unique generated-unit hashes, then record one approved hash-only review for the duplicate hash. On the current implementation, `review --all` reports `All clear`: both duplicate generated units independently accept the same coarse record through `matching_record_indices_for_block`.

The corrected behavior is that the parent remains unreviewed. `inspect --fingerprint <parent-hash> --split --coverage` must show the coarse record as linked, not direct, on both duplicate units; its binding relation must be absent; and its diagnostic must be `AmbiguousRecord` with both `README.md` path/hash/line/byte-span locations. A parallel path-scoped case, created with `mark --path README.md` and no `--line`, must remain ambiguous for the same two in-file candidates.

Controls in the same fixture establish that this is cardinality enforcement rather than blanket rejection:

- a hash-only record for the unique generated unit binds as `HashOnly` and directly approves it;
- an exact `--path README.md --line <first-start-line>` record for the duplicate hash binds as `Exact` and approves only the first distinct-line duplicate;
- the second duplicate stays uncovered until its own exact location record is added; and
- once both duplicate locations and all other generated units are approved, the parent becomes covered and review reports `All clear`.

### Same-line byte-distinct regression

In `trueflow/src/coverage.rs` tests, create a source-backed over-limit Markdown paragraph containing `Twin. Twin. Tail.` on one line. The two `Twin. ` generated sentences must have:

- equal content hashes;
- equal zero-based start lines;
- distinct, non-overlapping absolute `ByteSpan` values; and
- distinct internal `CoverageBlockLocator` values.

Assert that hash-only and path-only records are ambiguous. Also assert that a persisted path+hash+start-line record is ambiguous because its exact lookup produces both byte-distinct candidates; the implementation must not silently bind it to the first unit. The diagnostic must contain both complete byte spans. Keep the distinct-line integration control above to prove that an exact path+hash+line record still binds when the persisted tuple is unique.

### Diff filter-order regression

In `trueflow/src/commands/review.rs` tests, construct a diff review file containing two changed source-backed parent blocks whose generated review units share a hash. Give the parents different display kinds so one can be hidden by `only`, `exclude`, and the existing default import/whitespace display policy while the other remains visible. Assert fixture preconditions: both changed parents exist before filtering, both generated candidates have distinct complete locators, and the coarse record sees both.

For each filter mode, build coverage from the complete changed-block collection, then assemble the visible review summary. The hidden block must not appear or contribute to `total_blocks`, but its generated unit must remain in the candidate universe. The same coarse record therefore remains ambiguous and cannot clear the visible parent. This test fails if `collect_diff_review_files` filters blocks before `build_tree_from_diff_review_files` and `CoverageIndex::build`.

## Root cause

The index has two binding models:

1. Tree nodes participate in global exact/path/hash lookup tables. `TreeCoverageLookups::bind_block_record` computes one candidate set, checks its cardinality, stores one `RecordBinding` on success, and emits an ambiguity diagnostic without direct coverage on multiple matches.
2. Generated blocks are handled after index construction. `CoverageIndex::block` calls `matching_record_indices_for_block`, and `BlockCoverage::direct_latest_verdict_for` calls `record_match_relation_for_block` again. Those helpers answer only “does this record match this queried block?” They cannot answer “how many logical units could this record identify?”

Two representation/order defects would survive a naïve fix:

- a candidate key of path+hash+start-line would collapse byte-distinct same-line units before cardinality is checked, recreating issue #18 inside coverage;
- diff review currently removes disallowed/default-hidden changed blocks before the tree/index exists, so its supposedly global universe varies with presentation filters.

The defect is not Markdown parsing or latest-verdict precedence. It is the absence of every complete source occurrence from one filter-independent binding universe at the moment record cardinality is resolved. Review and inspect then consume a local match that contradicts the index's global `record_bindings` and `diagnostics`.

## Implementation plan

1. **Land and verify Issues #18 and #19 before starting this issue.**
   - Require issue #18's source-backed `Block::{start_byte, end_byte}`, `Block::byte_span()`, complete tree lookup identity, and checked parent-relative sub-block translation.
   - Require issue #19's source-exact optimizer output before deriving any generated locator. A `ByteSpan` attached to fabricated concatenated content is not a trustworthy occurrence identity.
   - Do not add temporary inferred spans, content-length offsets, line-only locators, optional fallbacks, or compatibility shims in this issue.

2. **Add all observable red tests before changing coverage behavior.**
   - In `trueflow/tests/sub_block_coverage.rs`, add a fixture helper for the over-50-line single-paragraph `README.md`. Return the parent hash, duplicate hash/line/byte-span locations, and at least one unique generated hash.
   - Add `test_hash_only_approval_does_not_cover_duplicate_generated_review_units`. Approve every unique generated unit with a coarse hash-only record, add one hash-only approval for the duplicated hash, and assert that review retains the parent rather than reporting `All clear`.
   - In that test, call `inspect --fingerprint <parent> --split --coverage` and assert for both duplicate locations: no direct approved verdict, the coarse record is linked with no binding relation, and one ambiguity diagnostic names the record and both candidate path/hash/line/`ByteSpan` locations. Assert the unique-hash control is directly approved with `HashOnly` and no ambiguity diagnostic.
   - Add the distinct-line exact-location control in TDD order: record `--path README.md --line <first-line>` for the duplicate hash and assert only that location becomes directly approved with `Exact`; then add the second exact record and assert review becomes clear.
   - Add `test_path_scoped_approval_does_not_cover_duplicate_generated_review_units` using a fresh fixture. Supply `--path README.md` without `--line` for the duplicate hash and assert a path-scoped ambiguity, no direct approval on either duplicate, and an uncleared parent.
   - In `trueflow/src/coverage.rs`, add `generated_review_unit_binding_keeps_same_line_byte_distinct_candidates`. Use the actual Markdown sub-splitter and assert two same-hash/same-line units remain distinct by complete byte span. Hash-only, path-only, and persisted path+hash+line records must all remain ambiguous; the diagnostic must contain both byte spans.
   - In `trueflow/src/commands/review.rs`, add `diff_coverage_candidate_universe_precedes_display_filters`. Exercise `only`, `exclude`, and an existing default display skip over two changed parents with same-hash generated candidates; hiding one parent must not make the coarse record unique.

3. **Introduce one complete identity for tree-backed and generated block candidates in `trueflow/src/coverage.rs`.**
   - Add an internal `CoverageUnitId` enum with tree-node and generated-block cases.
   - Define hashable `CoverageBlockLocator` as normalized `RepoPath` + block hash + complete absolute `ByteSpan`. Do not include start line as a substitute for either byte endpoint. This is the canonical logical identity used by `CoverageIndex::block`.
   - Add a serializable candidate description using explicit enum/struct composition. A block candidate must include path, hash, line span for human anchoring, and the complete byte span; a non-block tree target can retain its real `TreeNodeId`/path/hash representation. Never encode a generated block as a fake `TreeNodeId`.
   - Generalize `RecordBinding`, per-unit direct/linked facts, and block lookup maps from `TreeNodeId`-only storage to `CoverageUnitId`. Preserve node-oriented APIs by mapping a tree block node to its unit ID, while file/tree targets continue to bind to their actual tree nodes.
   - Deduplicate only by the complete `CoverageBlockLocator`. If `sub_splitter` returns an identity review unit equal to its parent tree block in path, hash, and byte span, reuse the tree-backed unit. Equal path/hash/start-line with a different byte span is not an identity split and must remain a second candidate.

4. **Build the complete candidate universe before binding records or applying display filters.**
   - Extend the lookup-construction phase currently implemented by `TreeCoverageLookups::from_tree` to register every existing tree block first, then derive generated units from each tree block's stored source-exact `Block` and `Language`.
   - Register only outputs whose `SubSplitResult.semantics` is `ReviewUnits`; do not convert `StructuralChildren` into approving units or alter their conservative review behavior.
   - Register the complete workdir/revision tree before `only`, `exclude`, whitespace/import/container defaults, or summary filtering. Candidate cardinality must not depend on which units a consumer elects to display.
   - Refactor diff collection specifically: `collect_diff_review_files` must retain every changed `DiffReviewBlock` returned by `collect_diff_review_blocks_for_file`. Remove its `allows_block`, `should_skip_whitespace_only_by_default`, and `should_skip_imports_by_default` filter before `build_tree_from_diff_review_files`.
   - Build the diff tree, generated-unit registry, and `CoverageIndex` from that complete changed-block set. In the later `collect_diff_scoped_review` loop, apply `allows_block`, whitespace/import defaults, and container default before incrementing `total_blocks` or populating unreviewed/display node sets. Hidden blocks remain in the tree/index only as cardinality candidates; they do not enter the visible summary.
   - Keep path/diff target selection as the review scope; the ordering requirement is that all changed blocks inside that selected scope precede block-kind/default display filtering.
   - For splitter failures, preserve current conservative behavior: keep the tree candidate, register no generated candidates for that split, and allow the parent to remain uncovered. Do not introduce a permissive local fallback.
   - Populate four deterministic structures: internal locator lookup `(path, hash, ByteSpan)` for `CoverageIndex::block`; persisted exact lookup `(path, hash, start_line) -> Vec<CoverageUnitId>`; path-scoped lookup `(path, hash)`; and hash-only lookup `(hash)`.

5. **Resolve each block record exactly once against the combined lookup tables.**
   - Update `bind_block_record` so `Exact`, `PathScoped`, and `HashOnly` all use combined tree/generated candidate IDs and the existing unique-candidate rule.
   - A hash-only record binds only when its hash names one logical block unit globally. A path-only record binds only when path+hash names one logical block unit in the normalized path candidate set.
   - A persisted exact record binds only when path+hash+start-line names one candidate. Duplicate hashes on distinct lines still bind exactly; two byte-distinct units with the same path/hash/start-line remain ambiguous because review JSONL has no byte hint.
   - On multiple candidates, attach the record only to each candidate's linked facts, store no direct binding, set no binding relation, and emit one `AmbiguousRecord` containing all complete candidate descriptions. On one candidate, populate direct and linked facts once and store the chosen `BindingRelation`.
   - Extend the existing `hash_only_block_record_is_reported_as_ambiguous` coverage without weakening its two-tree-node assertions. Give all new binder tests the `generated_review_unit_binding_` prefix so validation can stay focused.

6. **Remove the independent generated-block matching path.**
   - Change `CoverageIndex::block` to resolve the complete `CoverageBlockLocator` in the prebuilt registry and read that unit's direct and linked facts. Preserve `resolved_node_id` for tree-specific subtree/container behavior and use issue #18's byte containment for the smallest covering tree node of a generated unit.
   - Delete `matching_record_indices_for_block`; it is the per-query path that permits one coarse record to become direct coverage more than once.
   - Stop calling `record_match_relation_for_block` from `BlockCoverage::direct_latest_verdict_for`. Latest-verdict selection must consider only record indices already directly bound to that unit and use the one stored `RecordBinding` relation. Delete `record_match_relation_for_block` if it has no remaining caller.
   - Keep direct/effective verdict precedence, identity counting, file/tree inheritance, and record timestamp ordering unchanged.

7. **Make review and inspect consume the same precomputed facts and diagnostics.**
   - In `trueflow/src/commands/review.rs`, keep `is_subblock_covered`'s `ReviewUnits` all-units requirement, but make each `coverage.block(...)` call only a complete-locator lookup. It must not bind records locally or suppress ambiguity diagnostics.
   - In `trueflow/src/commands/inspect.rs`, keep `build_coverage_summary` as the shared presentation over `BlockCoverage`, but update serialization/assertions for the complete diagnostic candidate. `direct_reviews`, `linked_reviews`, `binding_relation`, verdict summaries, and diagnostics must all come from the same stored result.
   - Verify that inspecting either distinct-line duplicate reports the same ambiguous record/candidate set that prevented review coverage, while unique and exact controls report successful `HashOnly` or `Exact` binding. The same-line unit test protects the stricter persisted-exact ambiguity which the current record schema cannot express through CLI selection.
   - Do not add command-specific matching logic to review, inspect, or diff review.

8. **After the focused smoke passes, perform cleanup.**
   - Remove obsolete imports/helpers and update existing diagnostic pattern matches to the new candidate field.
   - Retain no aliases, legacy diagnostic variants, line-only internal locators, compatibility layers, or generated-node shims because the project is beta.

### Ordered affected files and symbols

1. `trueflow/tests/sub_block_coverage.rs`
   - Distinct-line Markdown generated-unit fixture.
   - Hash-only and path-scoped ambiguity regressions.
   - Inspect diagnostics, unique-hash control, and exact-location progression.
2. `trueflow/src/coverage.rs`
   - `CoverageDiagnostic` and complete candidate descriptions.
   - New `CoverageUnitId` and byte-span-aware `CoverageBlockLocator`.
   - `CoverageIndex`, `RecordBinding`, generalized per-unit facts, and combined lookup tables.
   - `CoverageIndex::build`, `CoverageIndex::block`, and block-record binding.
   - Removal of `matching_record_indices_for_block` and `record_match_relation_for_block`.
   - Existing duplicate-hash tests plus distinct-line and same-line generated-unit controls.
3. `trueflow/src/commands/review.rs`
   - `collect_diff_review_files` retains complete changed blocks.
   - `collect_diff_scoped_review` applies display filters only after tree/index construction.
   - `is_subblock_covered` consumes pre-bound generated-unit facts.
   - Focused diff filter-order/cardinality regression.
4. `trueflow/src/commands/inspect.rs`
   - `build_coverage_summary` and `diagnostic_record_id` present the same binding/diagnostic result and complete candidate shape.

## Verification and validation

Run only these focused commands from the repository root, in order.

1. Prove the primary distinct-line regression is red before the source change, then green afterward:

   ```sh
   cd trueflow && cargo test --features tui-test-support --test sub_block_coverage test_hash_only_approval_does_not_cover_duplicate_generated_review_units
   ```

2. Prove path-scoped records use the same cardinality rule:

   ```sh
   cd trueflow && cargo test --features tui-test-support --test sub_block_coverage test_path_scoped_approval_does_not_cover_duplicate_generated_review_units
   ```

3. Exercise generated-unit binding, unique/exact controls, existing tree ambiguity assertions incorporated by the new tests, and the same-line byte-distinct case:

   ```sh
   cd trueflow && cargo test --lib generated_review_unit_binding_
   ```

4. Prove diff candidate construction precedes `only`, `exclude`, and default display filtering:

   ```sh
   cd trueflow && cargo test --lib commands::review::tests::diff_coverage_candidate_universe_precedes_display_filters
   ```

5. When diagnosing the integration fixture, perform this focused JSON check:

   ```sh
   cd trueflow && cargo run -- inspect --fingerprint <parent-hash> --split --coverage
   ```

   Confirm both distinct-line duplicates list the same coarse record only under linked reviews, have no direct approved verdict or binding relation from that record, and expose one ambiguity diagnostic containing both complete byte spans. Confirm the unique generated hash has direct `HashOnly` coverage and a persisted exact record has `Exact` coverage only when its path+hash+start-line tuple is unique.

6. Run the final repository gate after the focused behavior is green and cleanup is complete:

   ```sh
   just check
   ```

## Acceptance criteria

- Issues #18 and #19 are complete before issue #20 begins; coverage never infers byte identity from line numbers, content length, or fabricated optimizer content.
- Generated `ReviewUnits` are registered in the same precomputed candidate-cardinality and record-binding universe as tree-backed blocks.
- Internal block occurrence identity is normalized path + content hash + complete absolute `ByteSpan`.
- One hash-only record cannot directly approve two same-hash generated units; both remain uncovered and the record is diagnosed as ambiguous.
- One path-scoped record cannot directly approve two same-path, same-hash generated units; both remain uncovered and the record is diagnosed as ambiguous.
- Ambiguous records remain linked to every candidate for explainability but contribute no direct verdict, identity count, or parent-clearing coverage.
- Ambiguity diagnostics identify the record and every conflicting block location, including path, hash, line span, and complete byte span.
- A unique generated-unit hash still accepts a hash-only record and reports `BindingRelation::HashOnly`.
- An exact path+hash+line record for one distinct-line duplicate reports `BindingRelation::Exact`, approves only that location, and does not approve its same-hash sibling.
- Two byte-distinct same-hash units beginning on the same line remain separate internal candidates; a persisted path+hash+line record is ambiguous for them because it has no byte discriminator.
- The Markdown parent remains unreviewed until every generated review unit has its own unambiguous approval; once both distinct-line duplicate locations and all controls are approved, review reports `All clear`.
- Workdir, revision, and diff cardinality are independent of `only`, `exclude`, and default display skips.
- The diff tree/index contains all changed blocks in the selected diff/path scope before filtering, while hidden blocks do not appear in the summary, `total_blocks`, or visible unreviewed/commented node sets.
- `review` and `inspect --split --coverage` agree because direct/linked records, binding relation, latest verdict, and diagnostics all come from the same stored binding result.
- Identity splits are deduplicated only when path, hash, and complete byte span equal the tree parent; same-line byte-distinct units are never collapsed.
- Existing duplicate-tree-node ambiguity and precise distinct-line approval behavior remain green.
- Every focused command above and the final `just check` gate pass.

## Non-goals and risks

### Non-goals

- Do not reject or weaken exact path+hash+line approvals when that persisted tuple is unique.
- Do not invent byte hints in the review-record schema. Same-line byte-distinct exact ambiguity is the honest limit of the current persisted target.
- Do not change the review-record schema, `mark` CLI, record persistence, or existing records; no migration or compatibility shim is needed.
- Do not reimplement issue #18 byte-span propagation or issue #19 optimizer provenance inside coverage.
- Do not change hash computation, Markdown sentence splitting, review-unit size thresholds, or review ordering.
- Do not change which blocks `only`, `exclude`, import/whitespace/container defaults display; only move diff filtering after candidate binding.
- Do not make `StructuralChildren` sufficient to cover an omitted container header.
- Do not change file/tree inherited coverage, latest-verdict precedence, two-person identity counting, or comment/rejection semantics.
- Do not resolve ambiguity by selecting the first, nearest, lowest-byte, or latest candidate.

### Risks and mitigations

- **False deduplication of same-line units:** path+hash+line is incomplete. Key internal occurrences and identity-split deduplication by the full issue #18 byte span.
- **False ambiguity from identity splits:** a splitter often returns the parent unchanged. Reuse the tree-backed unit only when path, hash, and both byte endpoints match.
- **Persisted exact limitation:** two candidates can share path/hash/start-line. Keep the exact lookup multi-valued and diagnose ambiguity; do not pretend a line hint contains byte precision.
- **Untrustworthy optimized spans:** generated translation is valid only when parent content is the exact source slice promised by issue #19. Enforce prerequisite ordering.
- **Consumer-dependent cardinality:** build candidates before visible filters. In diff review, retain complete changed blocks through tree/index construction and move display skips to the later summary loop.
- **Hidden diff nodes leaking into UI state:** keep the full tree for binding, but populate `total_blocks`, `unreviewed_block_nodes`, `commented_block_nodes`, and summary blocks only after display filtering; protect this with the focused diff regression.
- **Review/inspect drift:** remove both fallback helpers and require every consumer to read precomputed per-unit facts.
- **Diagnostic representation churn:** `TreeNodeId` cannot represent generated units. Use one explicit serializable candidate type with complete spans and update all pattern matches/tests in the same beta cutover.
- **Split failure behavior:** on a split failure, register no generated units for that split and retain conservative uncovered behavior.
- **Duplicate insertion or unstable diagnostics:** centralize registration in a complete-locator map and preserve deterministic tree/split insertion order.
- **Accidental performance regression:** build exact/path/hash indexes once and make `CoverageIndex::block` a complete-locator lookup; never scan all units or records per query.

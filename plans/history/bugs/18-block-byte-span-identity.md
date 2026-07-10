# Issue #18: Preserve block byte spans for same-line identity and parentage

Status: ready
Date: 2026-07-10
Baseline commit: `9a98914698c4`

## Problem

`Block` preserves only a 0-based, half-open line range. That is insufficient positional information for valid blocks which begin on the same line:

- two blocks with identical normalized content have the same `TreeHash`;
- two blocks on one line have the same `start_line` and `end_line`;
- `tree.rs::build_block_lookup_indexes` therefore cannot give those blocks distinct lookup keys;
- line-only containment also makes a later same-line container appear to be inside an earlier sibling container.

The failure is not a hash collision in the cryptographic sense. `TreeHash::from_content` intentionally hashes canonicalized content, so equal block text must continue to produce equal hashes. A source-backed block's in-memory identity must instead combine its repository path, content hash, and absolute source byte span. The byte span is 0-based and half-open: `[start_byte, end_byte)`.

The change must preserve absolute spans through top-level parsing, textual/fallback splitting, review-time optimization, sub-block construction, tree lookup, parent selection, coverage resolution, and serialized block views. It must not replace a uniqueness assertion with silent overwrite or multi-value lookup while leaving the model unable to identify positions.

Issues #18 and #19 form one atomic implementation stack. Issue #18 introduces and propagates the positional model; issue #19 must make **every** optimizer merge source-aware and source-exact before the stack is released or allowed to write the new scan-cache format. There is no supported intermediate deliverable in which a block advertises an absolute span while its `content` is a concatenation that differs from that source slice. After the #18+#19 stack is green, issue #20 may register tree and generated review units using the complete byte-span identity.

## Evidence

- `trueflow/src/block.rs::Block` stores `start_line` and `end_line` but no byte offsets. Its hash comment currently calls `hash` the block's “content-addressable identity,” even though `trueflow/src/hashing.rs::TreeHash::from_content` hashes only canonicalized text.
- `trueflow/src/block.rs::ByteSpan` already models `start_byte` and `end_byte`, but `contains` and `overlaps` are test-only and `ByteSpan` is not serializable or usable as a production lookup key.
- `trueflow/src/block_splitter.rs::split_tree_sitter` receives exact tree-sitter `start_byte()`/`end_byte()` values and passes them to `create_block_with_complexity`. That constructor derives line numbers and then discards the byte positions.
- Registered custom splitters under `trueflow/src/languages/` likewise calculate exact file-relative byte ranges in their `create_file_block` helpers, while their `create_sub_block` helpers calculate only line offsets relative to a parent.
- `trueflow/src/text_split.rs::split_by_paragraph_breaks` already passes byte offsets from `regex::Match::start`/`end`. `block_splitter.rs::create_fallback_block` derives lines from those offsets and discards the offsets, so text and parser fallback blocks can carry exact spans without rescanning or a new dependency.
- `trueflow/src/sub_splitter.rs::create_sub_block_with_kind` receives both `start_offset` and `end_offset`, but names the latter `_end_offset` and discards it. It derives only line numbers. The same pattern is duplicated in the registered language sub-splitters.
- `trueflow/src/block.rs::FileState::new` sorts blocks only by `(start_line, Reverse(end_line))`. Blocks which share a line therefore rely on incidental stable input order rather than explicit source position.
- `trueflow/src/tree.rs::Tree::block_nodes_by_path_hash_start`, `Tree::find_block_node`, `Tree::insert_block_child`, and `build_block_lookup_indexes` key a block by path, hash, and `start_line`. `build_block_lookup_indexes` asserts that this incomplete key is unique.
- The current assertion reports `duplicate block lookup key: path=src/lib.rs, hash=c2101244d572254e9520720fd0aec55e029ef707d2f8e61d70f1523d2f6946f3, start_line=0` for the repeated unnamed Rust constants described below. The second valid block triggers the panic.
- `trueflow/src/tree.rs::build_tree_from_files` and `trueflow/src/commands/review.rs::build_tree_from_diff_review_files` both maintain container stacks and choose parents with line-only containment. Their pop conditions also use `start_line > end_line`, although the line model documents `end_line` as exclusive.
- `trueflow/src/coverage.rs::CoverageIndex::smallest_covering_block_node` chooses containers by line containment and line length. `CoverageIndex::block` first delegates exact resolution to the incomplete tree lookup.
- `trueflow/src/commands/review.rs::dedupe_blocks` and its diff-tree ordering use hash and line coordinates, so same-line blocks can be collapsed or ordered ambiguously before the tree is built.
- `trueflow/src/commands/tui.rs::is_identity_subblock` compares kind, lines, and hash. Equal-content siblings on one line can therefore be mistaken for an identity sub-block.
- `Block` is serialized directly by scan and inspect JSON, scan cache entries, and feedback JSON. `Tree::view_json` manually serializes tree nodes without block positions, and feedback XML manually emits line positions only. Adding source position needs one deliberate beta schema cutover rather than aliases or fabricated defaults.
- `trueflow/src/feedback_export.rs` also constructs `Block` values for unresolved historical display context. Those values are not backed by a complete source file and cannot truthfully claim an absolute file byte span; they need a view representation rather than sentinel offsets in the source-backed model.
- `trueflow/src/optimizer.rs::flush_blocks` currently concatenates block strings and can insert a synthetic separator, while `optimize_small_files` concatenates strings without restoring source bytes omitted by the structured splitter. Import, module, code-paragraph, and small-file composites can therefore have content offsets that do not correspond to the bounding source interval.
- `BlockSplitResult::into_review_blocks` currently gives the optimizer only `Vec<Block>`, not the original source. A truthful byte-span model requires the exact source borrow to cross this boundary and reach every merge pass.
- Existing integration helpers named `expand_block_for_review_splitting` mutate only `end_line` in registered-language tests and `sub_block_semantics.rs`. After byte spans become required, those fixtures would compile while representing an impossible source range and could mask parent-relative offset defects.
- `DiffBlockSides::display_block` prefers the head block, but a diff tree may combine a changed parent (displayed with head coordinates) and a deleted child (available only in base coordinates). Comparing those byte spans directly crosses coordinate spaces and cannot prove containment.
- `feedback_export.rs::scoped_block_for_record` replaces a canonical block's content and line range with stored comment context. Even when the canonical block was scanned, the scoped presentation value has no byte span unless that context is uniquely recovered as an exact source slice.

## Reproduction

### Identical Rust blocks panic during tree construction

Use this valid single-line Rust source:

```rust
const _: () = (); const _: () = (); const _: () = ();
```

At the splitter boundary, the expected raw/review blocks are three `BlockKind::Const` values:

- all have content `const _: () = ();` and the same content hash `c2101244d572254e9520720fd0aec55e029ef707d2f8e61d70f1523d2f6946f3`;
- all have line span `[0, 1)`;
- their byte spans are `[0, 17)`, `[18, 35)`, and `[36, 53)`.

`block_splitter::split(..., Language::Rust).into_review_blocks()` preserves three blocks because three declarative blocks are not collapsed by the current small-file optimization. Building a `FileState` and calling `tree::build_tree_from_files` then panics on the second insertion because the current lookup key is only `(path, hash, start_line)`.

The red regression test must not use `#[should_panic]`. It must call the real splitter and tree builder, then assert that all three blocks exist as distinct `TreeNodeId` values even though their hashes and line spans are equal.

### Java same-line siblings receive the wrong parent

Use this valid single-line Java source:

```java
class Outer { class A {} class B {} void x() {} void y() {} }
```

The two methods keep the current optimizer from collapsing the file into one small declarative block. The splitter emits `Outer` and its immediate nested members in source order. Because `Outer`, `A`, `B`, `x`, and `y` all occupy line span `[0, 1)`, the current container stack treats `B` as contained by `A`; later members can then be attached below the wrong sibling as well.

The red parentage test must locate nodes by their source byte spans/content and assert:

- `A` and `B` are direct children of `Outer`;
- `x` and `y` are direct children of `Outer`;
- `A` and `B` are siblings, not ancestors of one another;
- no member is parented merely because it shares `Outer`'s line span.

## Root cause

Line ranges are display and line-anchoring metadata, not a complete source coordinate. Many different syntax nodes can map to the same half-open line interval. The model currently throws away the byte coordinate which tree-sitter and the textual splitters already computed, then compensates by assuming `(path, hash, start_line)` is unique and that line containment implies syntax containment.

That assumption fails in two independent ways:

1. **Lookup identity:** canonicalized content hash plus starting line does not distinguish repeated identical syntax on one line.
2. **Hierarchy:** inclusive comparison of coarse line ranges makes disjoint same-line siblings appear nested.

The fix is to retain the original source coordinate, not to alter content hashing. `TreeHash` remains a content fingerprint used for review matching and tree hashing. The source-backed lookup identity becomes `(RepoPath, TreeHash, ByteSpan)`, and hierarchy uses proper byte containment.

The source-coordinate invariants are:

- byte offsets are offsets into the original UTF-8 file bytes, never Unicode scalar counts or offsets into a copied substring;
- spans are 0-based and half-open, so `start_byte <= end_byte`, adjacent spans may satisfy `left.end_byte == right.start_byte`, and adjacency is neither overlap nor containment;
- source-backed blocks with non-empty content have `start_byte < end_byte`;
- both endpoints are UTF-8 character boundaries and are within the source length;
- **every** source-backed block, including an optimized composite, satisfies `source[start_byte..end_byte] == block.content`; there is no envelope-only exception;
- a sub-block parser may report offsets relative to `parent.content`, but the stored child span is absolute: `parent.start_byte + relative_start .. parent.start_byte + relative_end`, using checked arithmetic and verifying it stays within the parent span;
- parent-relative translation is legal only because the parent itself is an exact source slice; this is why the generalized issue #19 optimizer work is part of the atomic stack;
- a container is a source parent only when its byte span contains the child's and the spans are not equal; equal spans do not establish an arbitrary parent/child relation;
- line spans remain 0-based and half-open, are derived from the same exact byte slice, and remain available for display, diff, comments, and existing review-record line hints;
- `hash == TreeHash::from_content(&content)` remains independent of path and byte position. Equal content at different positions deliberately has the same hash.

Textual and fallback blocks are source-backed too. Paragraph regex matches, Markdown/TOML/Nix/Just split spans, unstructured code fallback ranges, and registered-language fallback gaps already have offsets into the full source; they must populate the same required absolute byte fields as tree-sitter blocks. There is no `None`, zero sentinel, content-local interpretation, or synthesized bounding envelope for a source-backed `Block`.

Detached feedback context is presentation data, not a source-backed block. It must move to an explicit feedback view whose source byte span is one `Option<ByteSpan>` (not two independently optional endpoints). Only an unmodified canonical scanned block, or a scoped context uniquely recovered and verified as the exact source slice for its content, receives `Some(ByteSpan)`. Resolved-but-rewritten scoped context and unresolved historical context receive `None`. These views must never enter `FileState`, `TreeBuilder`, lookup indexes, parent selection, sub-splitting, or coverage.

## Implementation plan

1. **Add all observable red tests before changing production behavior.**
   - In `trueflow/tests/tree_parent_blocks.rs`, add `test_scan_tree_keeps_identical_rust_blocks_on_same_line` using the three-constant Rust source. Assert the exact three byte spans, equal hashes/line spans, successful tree construction, and three distinct lookup results/node IDs.
   - In the same file, add `test_scan_tree_keeps_same_line_java_siblings_under_outer_class` using the `Outer`/`A`/`B`/`x`/`y` fixture. Assert the direct parent of every member and explicitly reject `A -> B` or `B -> A` nesting.
   - In `trueflow/src/block_splitter.rs` tests, add `tree_sitter_blocks_preserve_absolute_utf8_byte_spans` with multibyte text before and inside later syntax, and `fallback_blocks_preserve_absolute_utf8_byte_spans` for text/fallback input containing a multibyte paragraph and blank-line gap. Assert byte values with `str::find`, `source.is_char_boundary`, and exact source slicing; do not assert character counts.
   - In `trueflow/src/sub_splitter.rs` tests, add `sub_blocks_translate_relative_offsets_to_absolute_byte_spans`. Give the parent a non-zero absolute byte start and exact source content containing multibyte UTF-8; assert checked parent-relative translation and exclusive ends through at least two split levels.
   - In `trueflow/src/coverage.rs` tests, add `block_coverage_resolves_identical_same_line_blocks_by_byte_span` and `smallest_covering_block_uses_byte_containment_on_same_line`. Assert that `CoverageIndex::block(...).resolved_node_id()` selects the requested source position and that a same-line sibling is never chosen as a covering container.
   - In `trueflow/src/commands/review.rs` tests, add `diff_tree_parenting_uses_shared_side_byte_containment_for_deleted_child`. Shift a changed container's head bytes relative to its base, include a base-only deleted child, and prove the child attaches through proper base-side containment rather than by comparing its base span with the parent's head/display span.
   - In `trueflow/src/block.rs` tests, extend byte-span boundary coverage for empty/touching/proper containment and add a serialization test requiring numeric `start_byte` and `end_byte` fields.
   - Keep and update `tree::tests::build_tree_rejects_duplicate_block_lookup_keys`: it must still panic for a true duplicate with the same path, hash, and complete byte span.
   - Add the exact atomic-stack optimizer regressions owned by issue #19: `test_optimizer_source_integrity_preserves_python_small_file_delimiter`, `test_optimizer_source_integrity_preserves_disjoint_metadata`, `test_optimizer_source_integrity_rejects_nested_csharp_overlap`, `test_optimizer_source_integrity_preserves_omitted_import_gap`, and `test_optimizer_source_integrity_preserves_long_module_composite_child_provenance`; optimizer units `test_source_integrity_merge_slices_disjoint_source_and_metadata`, `test_source_integrity_merge_refuses_unprovable_candidate`, `test_source_integrity_flush_refuses_overlapping_whole_buffer`, `test_source_integrity_small_file_refuses_overlapping_whole_vector`, and `test_source_integrity_code_paragraph_merge_slices_source`; scanner unit `scan_cache_rejects_pre_source_integrity_format`; and existing `block_splitter::tests::test_split_includes_optimization_pipeline`. Keep the `test_optimizer_source_integrity_` and `optimizer::tests::test_source_integrity_` prefixes so their focused filters cannot silently match zero planned tests.
   - In feedback rendering tests, add separate source-backed, resolved-scoped, and unresolved cases. The source-backed block has a span; scoped/unresolved rewritten presentation has none unless the test proves a unique exact source recovery.

2. **Make source position a required part of the `Block` model.**
   - In `trueflow/src/block.rs`, add public, flat `start_byte: usize` and `end_byte: usize` fields beside the line fields. Flat fields keep scan/inspect JSON straightforward and mirror tree-sitter terminology.
   - Change `Block`'s hash documentation from “identity” to “content hash/fingerprint.” Add `Block::byte_span() -> ByteSpan` alongside `line_span()`.
   - Promote `ByteSpan::{contains, overlaps}` to production code, add `len`, `is_empty`, and `properly_contains`, and derive the serialization/equality/hash/order traits needed by JSON and `HashMap` keys.
   - Replace the ambiguous four-argument constructor with `Block::new(content, kind, LineSpan, ByteSpan)`. Require `content.len() == byte_span.len()` for source-backed construction; no call may use content length to infer an absolute start.
   - Add crate-visible source constructors which enforce one convention: `Block::from_file_range(full_source, kind, ByteSpan)` slices a file range and derives lines; `Block::from_parent_range(parent, kind, relative: ByteSpan)` slices the parent and translates to an absolute span with checked addition. Callers may then apply tags/complexity without rebuilding coordinates. Do not accept a separately supplied string that can disagree with the range.
   - Validate ordering, source bounds, UTF-8 boundaries, line derivation, and exact-slice equality at constructor boundaries. Do not use `saturating_add`, because saturation can turn malformed offsets into plausible but incorrect identities.

3. **Propagate file-absolute spans from every top-level splitter.**
   - In `trueflow/src/block_splitter.rs`, route `create_block`, `create_block_with_complexity`, `create_fallback_block`, Markdown helpers, and TOML/Nix/Just/paragraph fallback creation through the file-range constructor. Preserve the exact offsets already supplied by tree-sitter, parser spans, or `split_by_paragraph_breaks`.
   - Move or retain one byte-range-to-lines implementation behind the core constructor. Remove duplicated variants only after the focused behavior works.
   - Update all custom top-level `create_file_block`/gap constructors in `languages/{clojure,cpp,css,dart,elixir,go,haskell,html,json,lua,ocaml,scala,sql,yaml,zig}.rs`. Their tree-sitter or custom `SemanticSpan` offsets are already file-relative and must be stored unchanged.
   - Preserve attribute/comment-expanded starts. If content begins at a pending attribute/comment rather than the named node, the absolute span begins at that expanded start, matching the content and hash.

4. **Propagate absolute spans through all sub-splitters and remove impossible fixtures.**
   - In `trueflow/src/sub_splitter.rs::create_sub_block_with_kind`, stop discarding `_end_offset`. Treat both offsets as a half-open range relative to exact `parent.content`, use `Block::from_parent_range`, and store `parent.start_byte + offset` endpoints.
   - Ensure identity-return paths which clone the parent preserve its byte span unchanged.
   - Update every registered-language `create_sub_block` helper in `languages/{clojure,cpp,css,dart,elixir,go,haskell,html,json,lua,ocaml,scala,sql,yaml,zig}.rs` to use the same parent-range constructor while preserving tags and kind classification.
   - Replace every `expand_block_for_review_splitting` helper that inflates only `end_line` in `tests/e2e_{clojure,cpp,css,dart,elixir,go,haskell,html,json,lua,ocaml,php,scala,toml,yaml,zig}.rs`, `tests/e2e_elisp.rs`, and `tests/sub_block_semantics.rs`, and replace `trueflow/src/sub_splitter.rs::tests::make_large_block`, which creates the same contradiction internally. Call `sub_splitter::split_result_for_child_navigation` to force refinement without falsifying coordinates, or construct genuinely over-limit source and derive both line and byte spans from it. Remove obsolete `MAX_*_SPAN_LINES` imports.
   - In representative registered-language tests, assert each produced child equals the original file slice at its absolute span. Include nested child navigation and a non-zero parent start.
   - Update all remaining direct `Block` literals and `Block::new` calls to provide coherent line and byte spans. Test-only data must not retain contradictions merely because it compiles.

5. **Use complete positional identity and byte containment in ordinary trees.**
   - In `trueflow/src/block.rs::FileState::new`, sort by `(start_byte, Reverse(end_byte), start_line, Reverse(end_line))`. This puts an outer container before a child sharing its start and preserves deterministic source order for same-line siblings.
   - In `trueflow/src/tree.rs`, replace `block_nodes_by_path_hash_start` with an index keyed by path and a private key composed from `TreeHash` plus the complete `ByteSpan`. Use it in `build_block_lookup_indexes`, `Tree::find_block_node`, and dynamic `insert_block_child`.
   - Keep the uniqueness assertion. Its diagnostic includes path, hash, `start_byte`, and `end_byte`; a second insertion with the same composite identity remains an integrity error.
   - Change `build_tree_from_files`' container stack to retain `ByteSpan`. Pop at the exclusive boundary (`next.start_byte >= container.end_byte`) or when the top cannot properly contain the next block. Select a parent only with `properly_contains`; line equality has no effect.
   - Include byte spans in `commands/review.rs::dedupe_blocks`, base/head ordering, and final diff-block ordering. Do not collapse two equal-hash blocks merely because they share a line interval.

6. **Precompute and reconcile diff parents independently per source side.**
   - In `trueflow/src/commands/review.rs::build_tree_from_diff_review_files`, do not walk one stack in `display_block` order. Display order can interleave head-backed and base-only entries whose offsets belong to different files.
   - First assign stable logical entry IDs to the paired `DiffReviewBlock` values. Build a base-side ordering from only entries with `sides.base`, sorted by base `(start_byte, Reverse(end_byte), stable ID)`, and independently build a head-side ordering from only entries with `sides.head`, sorted by head coordinates.
   - Run the ordinary proper-containment stack separately over each ordering to produce `base_parent: entry -> entry` and `head_parent: entry -> entry`. Each map compares only spans from its own side, uses exclusive ends, and cannot depend on the eventual display-node insertion order.
   - Reconcile parent evidence before constructing `TreeNode`s. If the child has a head representation, use head-side parent evidence; matching base evidence may confirm it but cannot override the current head hierarchy. For a base-only/deleted child, use base-side evidence. If no parent exists on the child's selected side, attach it to the file rather than borrowing a coordinate from the other side. A parent is eligible only when that parent entry also has the shared side whose proper containment produced the edge.
   - Validate the reconciled parent relation as acyclic, compute parent depth, and insert entries topologically (parent before child), using stable source/entry order within a depth. This allows a shifted changed parent to be inserted before its base-only deleted child even when head/display byte order would place them differently.
   - Continue using `DiffBlockSides::display_block` only for the final node payload and label. Add the shifted changed-parent/base-only-child regression from step 1 and assert it is parented by the independently precomputed base map, never by a mixed base-to-head comparison.

7. **Propagate exact lookup and containment into coverage and other identity-sensitive consumers.**
   - In `trueflow/src/coverage.rs::CoverageIndex::block`, rely on byte-span-aware `Tree::find_block_node`.
   - Change `smallest_covering_block_node` to use `ByteSpan::contains` and minimize by byte length with deterministic byte-boundary tie-breakers. Equality may be considered only for coverage fallback after exact lookup fails; sibling overlap/touching is not containment.
   - Keep review-record hashes and path/line hints unchanged. `TreeCoverageLookups::block_exact` may diagnose a legacy line-hinted record as ambiguous when identical blocks occupy one line; it must never choose the first byte occurrence.
   - In `commands/tui.rs::is_identity_subblock`, require equal byte spans in addition to kind/hash/lines.
   - Audit in-memory identity/dedup tuples containing hash and lines. Add the complete byte span where they identify a source occurrence; keep line-only code that is explicitly display, comment, or diff-line logic.
   - Establish the handoff to issue #20: its generated `CoverageBlockLocator` is `(normalized path, content hash, complete ByteSpan)`, diagnostics include byte spans, and the complete unfiltered diff candidate universe is built before CLI block-kind filtering. Persisted exact records remain path+hash+start-line and are ambiguous when that names multiple byte-distinct units.

8. **Complete generalized source-exact optimization as issue #19 in the same atomic stack.**
   - Thread the exact borrowed source through `BlockSplitResult::into_review_blocks`, `optimizer::optimize`, and every consumer (`scanner::process_file`, `finder::fuzzy_find_block`, VCS splitting, and direct tests). Make a clean signature cutover.
   - Make `optimize_imports`, `optimize_modules`, `optimize_code_paragraphs`, and `optimize_small_files` validate monotonically ordered, pairwise-disjoint UTF-8 spans before merging. Overlap, reversal, invalid bounds, or unprovable provenance conservatively leaves candidates unmerged.
   - Build every accepted merge exactly once from `source[start_byte..end_byte]`; derive content, hash, lines, and byte span from that slice. Restore omitted whitespace from source and remove synthetic separator/content concatenation paths.
   - Aggregate tags and complexity only after disjointness is proven so each input contributes at most once.
   - Use the exact shared long-composite fixture from issue #19: 17 one-line `mod left_N;` declarations, one blank line, and 17 one-line `mod right_N;` declarations, with no trailing newline. The real pipeline must produce one 35-line `BlockKind::Modules` composite (above the 32-line review-unit limit and within the module optimizer's 48-line limit). Assert `start_byte == 0`, `end_byte == source.len()`, exact source content/hash/span, then assert both normal `split_result` and forced `split_result_for_child_navigation` return multiple children; every child, including gaps, has UTF-8 boundaries, is contained, equals `source[child.start_byte..child.end_byte]`, and preserves source order.
   - Do not release, cache, or run issue #20 against the byte-span model until this generalized issue #19 contract is green.

9. **Make serialization one clean post-#19 beta cutover and keep feedback views honest.**
   - `Block` JSON emitted by scan, inspect, tree, and source-backed feedback gains required numeric `start_byte`/`end_byte`. Do not add defaults, aliases, nullable endpoints, or a legacy `Block` deserializer.
   - In `Tree::view_json`, include line and byte endpoints on block nodes; directory/file nodes fabricate none.
   - Increment `SCAN_CACHE_FORMAT_VERSION` exactly once from its then-current value for the atomic #18+#19 result. The baseline is version 2; if issue #17 has already advanced it to 3, this stack advances 3 to 4 instead of reusing 3. No intermediate build may emit the selected new version before generalized source-exact optimization is present. Read the envelope version before deserializing `FileState`, making the immediately previous version a normal cold miss; do not migrate spans or optimizer output. The regression must derive or construct `SCAN_CACHE_FORMAT_VERSION - 1`, not hard-code a historical number.
   - In `feedback_export.rs`, introduce an explicit view separate from source-backed `Block`. Only an unmodified canonical scanned block gets `Some(ByteSpan)` by default. A scoped context whose content/lines were replaced gets `None` unless implementation uniquely recovers and verifies the exact slice; unresolved context gets `None`.
   - In feedback JSON/XML, emit both byte endpoints together only for `Some(ByteSpan)` and omit both for `None`. Add resolved-scoped coverage so copying the canonical block's span onto rewritten context fails.

10. **Run focused smoke only after the #18+#19 atomic implementation is complete, then clean up.**
    - Run only the exact regressions listed below before cleanup. A failure is a missed source boundary, constructor, side comparison, or fixture migration—not a reason to add defaults.
    - After focused smoke, remove duplicate byte-to-line calculations, obsolete line-inflation helpers, synthetic optimizer concatenation/separator paths, and stale line-identity names/comments.
    - Then run the final repository gate. Only after it passes may issue #20 build its complete byte-span-keyed, unfiltered coverage candidate universe.

### Ordered affected files and symbols

1. `trueflow/tests/tree_parent_blocks.rs` — Rust panic-free and Java parentage regressions.
2. `trueflow/src/block.rs` — `Block`, `ByteSpan`, source constructors, `FileState::new`, model/serialization tests.
3. `trueflow/src/block_splitter.rs` — top-level parser/fallback creation and source-aware `into_review_blocks`.
4. `trueflow/src/languages/{clojure,cpp,css,dart,elixir,go,haskell,html,json,lua,ocaml,scala,sql,yaml,zig}.rs` — file and parent-relative constructors.
5. `trueflow/src/sub_splitter.rs` — parent-relative translation, forced-child-navigation tests, and removal/replacement of the line-inflating unit-test `make_large_block`.
6. `trueflow/tests/e2e_{clojure,cpp,css,dart,elixir,go,haskell,html,json,lua,ocaml,php,scala,toml,yaml,zig}.rs`, `trueflow/tests/e2e_elisp.rs`, and `trueflow/tests/sub_block_semantics.rs` — remove line-inflated fixtures.
7. `trueflow/src/tree.rs` — complete lookup key, insertion, parent selection, tree JSON, uniqueness tests.
8. `trueflow/src/commands/review.rs` — byte-aware dedupe/order and same-side diff parent proof/regression.
9. `trueflow/src/coverage.rs` and `trueflow/src/commands/tui.rs` — exact lookup/covering container and identity sub-block comparison.
10. Atomic issue #19 files: `trueflow/src/optimizer.rs`, `block_splitter.rs`, `scanner.rs`, `finder.rs`, `vcs.rs`, and direct optimization tests — generalized exact-source merges and API propagation.
11. `trueflow/src/scanner.rs` — one post-#19 cache format/version gate and prior-version miss regression.
12. `trueflow/src/feedback_export.rs` and `trueflow/src/commands/feedback.rs` — canonical/scoped/detached view semantics and JSON/XML tests.
13. Remaining production/test `Block` constructors — mechanical coherent coordinate migration with no legacy overload.

## Verification and validation

Run these commands from the repository root in order. Keep pre-cleanup checks narrow; the only broad command is the required final repository gate.

### Issue #18 positional regressions

```sh
cd trueflow && cargo test --test tree_parent_blocks test_scan_tree_keeps_identical_rust_blocks_on_same_line -- --exact
cd trueflow && cargo test --test tree_parent_blocks test_scan_tree_keeps_same_line_java_siblings_under_outer_class -- --exact
cd trueflow && cargo test --lib block_splitter::tests::tree_sitter_blocks_preserve_absolute_utf8_byte_spans -- --exact
cd trueflow && cargo test --lib block_splitter::tests::fallback_blocks_preserve_absolute_utf8_byte_spans -- --exact
cd trueflow && cargo test --lib sub_splitter::tests::sub_blocks_translate_relative_offsets_to_absolute_byte_spans -- --exact
cd trueflow && cargo test --lib tree::tests::find_block_node_distinguishes_identical_hashes_on_same_line_by_byte_span -- --exact
cd trueflow && cargo test --lib tree::tests::build_tree_rejects_duplicate_block_lookup_keys -- --exact
cd trueflow && cargo test --lib coverage::tests::block_coverage_resolves_identical_same_line_blocks_by_byte_span -- --exact
cd trueflow && cargo test --lib coverage::tests::smallest_covering_block_uses_byte_containment_on_same_line -- --exact
cd trueflow && cargo test --lib commands::review::tests::diff_tree_parenting_uses_shared_side_byte_containment_for_deleted_child -- --exact
```

The uniqueness test passes by observing a panic only for a genuinely duplicated complete key. The repeated-Rust-block regression must be panic-free.

### Atomic issue #19 provenance regressions

Use exactly the names, prefixes, and fixture contract shared with plan #19. These checks must pass before serialization/cache validation or issue #20:

```sh
cd trueflow && cargo test --test bug_regressions test_optimizer_source_integrity_
cd trueflow && cargo test --lib optimizer::tests::test_source_integrity_
cd trueflow && cargo test --test bug_regressions test_optimizer_source_integrity_preserves_long_module_composite_child_provenance -- --exact
cd trueflow && cargo test --lib scanner::tests::scan_cache_rejects_pre_source_integrity_format -- --exact
cd trueflow && cargo test --lib block_splitter::tests::test_split_includes_optimization_pipeline -- --exact
```

The prefix commands intentionally cover the complete named families above. The long-module command uses 17 one-line `mod left_N;` declarations, one blank line, and 17 one-line `mod right_N;` declarations without a trailing newline. It asserts one source-exact 35-line `Modules` composite, then runs both normal and forced child navigation and proves every returned child's content is the original source slice at its absolute span.

### Fixture and serialized-view regressions

```sh
cd trueflow && cargo test --test sub_block_semantics test_registered_language_subblocks_preserve_source_spans -- --exact
cd trueflow && cargo test --test e2e_inspect test_inspect_split_reports_absolute_byte_spans -- --exact
cd trueflow && cargo test --lib commands::feedback::tests::source_backed_feedback_serializes_byte_span -- --exact
cd trueflow && cargo test --lib commands::feedback::tests::resolved_scoped_feedback_does_not_copy_canonical_byte_span -- --exact
cd trueflow && cargo test --lib commands::feedback::tests::detached_feedback_does_not_fabricate_byte_span -- --exact
```

For inspect and representative registered-language tests, use non-ASCII source and reconstruct every returned child from the original file bytes. The scoped feedback regression must replace content/lines and assert both byte endpoints are absent unless unique exact recovery is explicitly exercised.

### Behavioral checks

- The Rust fixture's tree JSON has three `Const` nodes with one hash/line range and spans `[0,17)`, `[18,35)`, `[36,53)`.
- The Java fixture gives `Outer` four direct members and no same-line sibling ancestry.
- A shifted changed diff parent plus base-only deleted child attaches by base-to-base proper containment, never base-to-head comparison.
- Every accepted optimizer composite, including import/module/code-paragraph/small-file cases with omitted whitespace, satisfies `source[span] == content` before any sub-splitting.
- Adjacent exclusive spans are non-overlapping siblings; exact duplicate positional keys are rejected.
- Equal content at different positions retains equal `TreeHash` values.
- Line-inflated force-split fixtures no longer exist; forced navigation keeps the canonical parent coordinates unchanged.
- Canonical feedback has a source span; rewritten scoped and unresolved feedback omit both endpoints unless an exact slice was uniquely recovered.
- The previous scan-cache version is a cold miss, and the new version is emitted only by the complete #18+#19 implementation.

### Final repository validation

```sh
just check
```

## Acceptance criteria

- The single-line Rust source `const _: () = (); const _: () = (); const _: () = ();` scans and builds a tree without panic.
- Its three constants retain equal hashes/line spans, exact byte spans `[0,17)`, `[18,35)`, `[36,53)`, and distinct tree nodes.
- The Java fixture keeps `A`, `B`, `x`, and `y` as direct children of `Outer`; same-line sibling containers are not nested by line coincidence.
- `Block` exposes required absolute `start_byte`/`end_byte` and `byte_span()`. No source-backed constructor omits or invents a span.
- Every source-backed block—tree-sitter, custom parser, text/fallback, gap, optimized composite, or recursive sub-block—is one exact UTF-8 source slice at its half-open span.
- Parent-relative sub-splitting uses checked addition, stays within an exact parent, preserves UTF-8 boundaries, and remains correct through nested/forced navigation.
- All line-inflated review-splitting fixtures are migrated to `split_result_for_child_navigation` or genuinely long source; line and byte coordinates are never intentionally contradictory.
- `FileState` ordering, tree lookup, dynamic insertion, in-memory dedupe, and ordinary parent selection use complete byte position.
- Diff-tree parentage is proven only by proper containment on a side shared by parent and child. A base-only deleted child is never compared with a changed parent's head/display span.
- Coverage resolves a `Block` by complete byte span and chooses covering containers by byte containment/length. A line-hinted persisted record that names multiple byte occurrences remains ambiguous.
- The uniqueness assertion remains and rejects a duplicate complete positional key; no insertion silently replaces a node.
- `TreeHash` remains a canonical content hash independent of path/position; equal content retains equal hashes.
- Issues #18 and #19 land as one atomic result. Every merge pass uses ordered, pairwise-disjoint provenance and exact source slicing; overlap/unprovable provenance is not merged.
- The post-stack scan-cache format is emitted only after generalized source-exact optimization and treats the previous version as a clean cold miss.
- Scan/inspect/tree/source-backed feedback serialization exposes truthful byte spans. Rewritten scoped and unresolved feedback views omit the whole span unless an exact source slice is uniquely recovered.
- Issue #20 begins only after #18+#19 pass. Its internal coverage identity includes normalized path, content hash, and complete byte span, and diff candidate cardinality is built before display filtering.
- All focused commands pass, followed by `just check`.

## Non-goals and risks

### Non-goals

- Do not weaken/remove duplicate-key assertions, accept last-write-wins, or hide an incomplete identity behind a vector.
- Do not salt hashes with path, line, byte offset, kind, or occurrence counters.
- Do not add aliases, optional source-block defaults, cache migrations, deprecated constructors, or intermediate beta compatibility shims.
- Do not change user-facing line numbering, comment anchors, GitHub mapping, or diff hunk semantics; bytes complement lines.
- Do not infer syntax hierarchy from block kinds when same-side source containment is available.
- Do not migrate review JSONL. Existing hash/path/line records remain ambiguous when they cannot identify one byte occurrence.
- Do not ship issue #18's model with envelope-only/synthesized optimizer composites. Issue #19 owns generalized source-exact merging and is an atomic release prerequisite.
- Do not let issue #20 use line-only generated-unit identity or a filter-dependent candidate universe.

### Risks and mitigations

- **UTF-8 confusion:** character counts can pass ASCII tests. Use non-ASCII parser, fallback, optimizer, and nested-sub-block fixtures with exact source slicing.
- **Off-by-one containment:** an inclusive end recreates sibling nesting. Test touching spans and pop at `next.start_byte >= container.end_byte`.
- **Relative/absolute mixing:** centralize parent translation and test non-zero absolute starts through multiple split levels.
- **Non-exact optimizer parents:** content concatenation invalidates `parent.start_byte + relative_offset`. Make all merge passes source-aware in atomic issue #19 and refuse overlap/unprovable provenance before exposing the new model.
- **Cross-side diff comparison:** display blocks mix base/head coordinate spaces. Store both sides or per-side stacks and require proper containment on one shared side.
- **Equal-span pseudo-parenting:** proper containment, not iteration order, establishes hierarchy.
- **Impossible test fixtures:** line-only inflation can mask bugs. Force child navigation through its explicit API without mutating canonical spans.
- **Scoped feedback leakage:** copying a canonical span onto rewritten context lies about provenance. Use `None` unless exact slice recovery is unique and verified.
- **Cache decode/order:** gate on envelope version before typed payload decoding, and emit the new version only from the complete #18+#19 implementation.
- **Partial constructor migration:** required fields should force completion; do not defeat this with defaults or a legacy overload.

# Issue #19: Optimizer source integrity for small-file and sequence merges

Status: ready  
Date: 2026-07-10  
Baseline commit: 9a98914698c4

## Problem

Every optimizer path that combines blocks can manufacture content that never existed in the scanned file. `optimize_small_files` concatenates every accepted `Block::content`, while `flush_blocks` concatenates the selected import/module/code-paragraph range and may inject a synthetic separator. Raw structured blocks are not guaranteed to form a delimiter-complete, flat partition of the source:

- the structured splitter intentionally omits whitespace-only gaps between syntax nodes, so adjacent blocks can require source bytes that are present in the file but absent from every block;
- structured language support can emit both a containing declaration and nested declarations, so blocks can overlap and appear in parent-then-child order.

Two observed failures demonstrate both shapes.

1. Python source `import os\nx = 1\n` splits into an `Import` containing `import os` and a `Code` block containing `x = 1`, with no `Gap` block for the intervening newline. The small-file pass accepts the pair and currently emits one `Code` block containing `import osx = 1`.
2. A small C# class containing one method with one `if` splits into the enclosing `Class` at lines `[0, 6)` with complexity `Some(1)` and the nested `Method` at lines `[1, 5)` with complexity `Some(1)`. The small-file pass currently appends the method after the complete class, uses the nested method's line 5 as the merged end even though the class ends at line 6, and sums the same control-flow complexity twice to `Some(2)`.

The same provenance defect exists before the small-file pass. `flush_blocks` builds `Imports`, `Modules`, and merged `CodeParagraph` blocks from child strings rather than their source envelope. An omitted blank-line gap is either normalized to the hard-coded import separator or dropped entirely. The resulting composite may have a plausible bounding byte span from issue #18 while `composite.content` has different offsets. Any later `Block::from_parent_range` or equivalent parent-relative sub-split translation then computes incorrect absolute child bytes.

The result reaches normal users: `scanner::process_file` reads the exact file bytes, calls `block_splitter::split`, immediately calls `BlockSplitResult::into_review_blocks`, and gives those optimized blocks to `FileState::from_text`. The corrupted block hashes therefore also contribute to the file's review-tree hash and can persist in the scan cache.

Issues #18 and #19 are one atomic correctness stack. Issue #18 is implemented first locally and establishes required absolute half-open UTF-8 byte offsets as `Block::{start_byte, end_byte}` plus `Block::byte_span()`. Issue #19 must make every optimizer-produced composite source-exact before that model is merged or released. No issue-#18-only artifact may ship or write the final cache format. Although the baseline constant is currently 2, issue #17 may increment it first; the final #18+#19 stack must read the then-current `SCAN_CACHE_FORMAT_VERSION` and increment it exactly once, never assume a fixed destination value.

## Evidence

- `trueflow/src/block_splitter.rs::split_tree_sitter` tracks `last_end_byte`, but only materializes an inter-node `Gap` when `!gap.trim().is_empty()` (currently lines 389-400). A newline-only Python gap is therefore deliberately absent from `BlockSplitResult::blocks`.
- The same splitter first pushes the top-level node slice (currently lines 405-413), then appends registered or language-specific nested blocks (currently lines 415-489). `collect_csharp_declaration_blocks` and `collect_csharp_type_items` recursively emit the class and its method (currently lines 1676-1709 and 1755-1769), so their source ranges overlap by design.
- `trueflow/src/optimizer.rs::optimize` runs imports, modules, code paragraphs, and then small files (currently lines 11-15), so a composite from an earlier pass can be consumed by the small-file pass or later sub-splitting.
- `flush_blocks` selects the first-through-last target range, optionally injects a separator, appends each block string, and constructs a composite from that assembly (currently lines 199-250). The import pass passes `Some("\n")` (currently line 62); module and code-paragraph passes pass `None` (currently lines 93-100 and 133).
- `optimize_small_files` performs an unconditional `push_str` for every accepted block (currently lines 142-145), takes the first block's start line and last block's end line (currently lines 147-150), and aggregates metadata (currently lines 151-153).
- `should_merge_small_file` checks counts, kinds, logic-block count, and a line-span threshold (currently lines 272-326), but it does not check source order, byte-range validity, missing delimiters, or overlap. `flush_blocks` performs no provenance validation either.
- `merged_metadata` preserves tags by first occurrence and saturating-adds every available complexity value (currently lines 253-269). That behavior is sound only when each input represents disjoint source; a class plus its nested method counts the method body twice.
- `Block::new` derives the block hash from the newly assembled content (currently `trueflow/src/block.rs` lines 260-269), so a fabricated concatenation receives a valid-looking content hash.
- `sub_splitter::split_result_for_child_navigation` forces refinement even below the normal line threshold (currently `trueflow/src/sub_splitter.rs` lines 97-118 and 134-170). TUI child expansion calls it and inserts the returned children (currently `trueflow/src/commands/tui.rs` lines 3230-3262). Parent-relative byte translation is only correct when the optimized parent content is the exact source slice represented by its absolute span.
- `scanner::process_file` reads `bytes` and `content`, then consumes `split_result.into_review_blocks()` before constructing `FileState` (currently `trueflow/src/scanner.rs` lines 858-883). `FileState::new` hashes the ordered child block hashes (currently `trueflow/src/block.rs` lines 368-373); it cannot recover the original source after optimization.
- `SCAN_CACHE_FORMAT_VERSION` is manually maintained at baseline value 2 (`trueflow/src/scanner.rs` line 19). `load_cache_entry` accepts a cache solely when its stored version, root hash, and options fingerprint match (currently lines 514-532); optimizer implementation changes are not independently fingerprinted.
- Existing optimizer unit tests construct delimiter-complete synthetic `Gap` blocks and therefore miss structured splitters' omitted whitespace, parent/child overlap, and optimized-parent sub-split provenance. Existing import assertions even expect normalized rather than source-exact spacing.

## Reproduction

From the repository root, add a regression that scans this Python file without the cache:

```python
import os
x = 1
```

Characterize the raw structured result before asserting optimized behavior:

- raw block 1: `Import`, content `"import os"`, bytes `[0, 9)`, lines `[0, 1)`;
- raw block 2: `Code`, content `"x = 1"`, bytes `[10, 15)`, lines `[1, 2)`;
- no raw block owns byte 9, the newline between the syntax nodes.

At the baseline, scanner output has one `Code` block containing `"import osx = 1"`. The correct accepted merge is the exact source slice `source[0..15]`, namely `"import os\nx = 1"`; the terminal newline at byte 15 lies outside both semantic endpoints and need not be absorbed.

Also scan this C# file:

```csharp
class Worker {
    int Run() {
        if (true) return 1;
        return 0;
    }
}
```

The observed raw blocks are:

- `Class`: bytes `[0, 84)`, lines `[0, 6)`, complexity `Some(1)`, content equal to `source[0..84]`;
- nested `Method`: bytes `[19, 82)`, lines `[1, 5)`, complexity `Some(1)`, content equal to `source[19..82]`.

At the baseline, the optimized block contains the entire class followed by another copy of the method, reports lines `[0, 5)`, and reports complexity `Some(2)`. The correct behavior is conservative refusal: retain the two original blocks and their independent content, hashes, byte and line bounds, tags, and complexity values.

The same red suite must cover the other composite passes:

- `use std::fmt;\n\nuse std::io;\n` must become one `Imports` block containing the exact slice `source[0..27]`, including both newline bytes, rather than the current normalized one-newline string;
- a 35-line Rust source containing 17 `mod left_N;` declarations, one blank line, then 17 `mod right_N;` declarations, with no trailing newline, must become one `Modules` block equal to the complete source; its 34 declaration spans and omitted newlines remain one exact interval within the module pass's 48-line limit and above the normal 32-line review-unit split threshold;
- both normal `sub_splitter::split_result` and TUI-style `split_result_for_child_navigation` on that composite must return multiple children for which `child.content == source[child.start_byte..child.end_byte]`.

Finally, a cache entry marked with the immediately previous scan-cache format and containing a stale fabricated optimizer block must be a clean cache miss under the incremented final format, followed by a rescan of the unchanged on-disk source.

The focused red commands are:

```sh
cd trueflow && cargo test --test bug_regressions test_optimizer_source_integrity_
cd trueflow && cargo test --lib scanner::tests::scan_cache_rejects_pre_source_integrity_format
```

## Root cause

The optimizer treats `Vec<Block>` as a flat, lossless partition, but the splitter contract is a semantic block collection:

- block content can exclude source delimiters because insignificant gaps are omitted;
- blocks can overlap because parent and nested review targets are both useful;
- line ranges do not identify the missing byte interval and cannot reliably detect same-line or nested overlap;
- vector order does not imply that the last block has the greatest source end.

Consequently, concatenating block strings is not a source reconstruction operation. First/last line selection and metadata summation compound the same invalid assumption. The fix must use issue #18's absolute byte provenance, validate a monotonically ordered disjoint candidate, and build accepted content from the original source rather than from constituent strings.

For every merge performed by any optimizer pass, the invariant is:

```text
merged.content == original_source[merged.start_byte..merged.end_byte]
```

The merged byte interval must be valid UTF-8, the participating input byte intervals must be ordered and pairwise disjoint, and every input source byte may contribute to the merged content and aggregated complexity at most once. Whitespace between disjoint blocks is included once by the source slice even if the splitter emitted no `Gap`. Overlapping, reversed, invalid, or otherwise unprovable candidates are not aggregated; the whole candidate remains unchanged. This invariant is required before parent-relative sub-splitting can safely translate local offsets to file offsets.

## Implementation plan

1. **Treat issues #18 and #19 as one atomic stack.** Implement issue #18's required absolute `Block::start_byte`/`end_byte` fields and `Block::byte_span()` first locally, but do not merge, release, or emit the final cache format from an issue-#18-only state. Issue #18 owns exact span propagation through source constructors and sub-splitters. Issue #19 owns source-aware optimization and guarantees that every optimizer composite's content is the exact source slice named by that span. The stack is complete only when both invariants hold together.

2. **Add scanner-level red regressions first in `trueflow/tests/bug_regressions.rs`.** Use the exact names below. All five share the `test_optimizer_source_integrity_` prefix used by the focused aggregate filter.
   1. `test_optimizer_source_integrity_preserves_python_small_file_delimiter` must first assert the raw Python split described above, including the absent byte-9 gap, and then scan the same file through `TestRepo::scan_without_cache`. Assert one optimized `Code` block with content exactly `&source[0..15]`, hash exactly `TreeHash::from_content(&source[0..15])`, bytes `[0, 15)`, lines `[0, 2)`, tags `[]`, and complexity `Some(0)`. Assert the `FileState::tree_hash` is derived from that exact block hash.
   2. `test_optimizer_source_integrity_preserves_disjoint_metadata` must scan `"import os\ndef test_run():\n    if True:\n        return 1\n"`. Assert that the accepted block is the exact source slice `[0, 55)`, has bytes `[0, 55)`, lines `[0, 4)`, kind `Code`, hash from that exact slice, tags exactly `["test"]`, and complexity exactly `Some(1)`.
   3. `test_optimizer_source_integrity_rejects_nested_csharp_overlap` must characterize the raw `Class` `[0, 84)`/`Method` `[19, 82)` pair and then assert scanner output retains exactly those two blocks in that order. Compare each block's kind, source-slice content, content hash, byte bounds, line bounds, tags `[]`, and complexity `Some(1)`. Assert the file tree hash is the ordered hash of the two retained hashes.
   4. Strengthen/rename the existing import regression as `test_optimizer_source_integrity_preserves_omitted_import_gap`. For `use std::fmt;\n\nuse std::io;\n`, require one `Imports` block with content and hash from exact source bytes `[0, 27)`, byte bounds `[0, 27)`, and lines `[0, 3)`. The current expectation that collapses two newlines to one is the bug, not a compatibility contract.
   5. Add `test_optimizer_source_integrity_preserves_long_module_composite_child_provenance` with exactly 17 one-line `mod left_N;` declarations, one blank line, and 17 one-line `mod right_N;` declarations, without a trailing newline. The real split/optimization pipeline must produce one `BlockKind::Modules` composite spanning 35 lines—above `MAX_REVIEW_UNIT_SPAN_LINES` 32 and within the module optimizer's 48-line limit—with `start_byte == 0`, `end_byte == source.len()`, exact source content, and `TreeHash::from_content(source)`. Call both normal `sub_splitter::split_result` and forced `sub_splitter::split_result_for_child_navigation`; each must return multiple children. For every child, including any `Gap`, assert UTF-8 boundaries, containment inside the composite, and `child.content == &source[child.start_byte..child.end_byte]`. Non-gap children must preserve the two declaration groups in source order without synthesized or deleted bytes.

3. **Thread the original source through the complete optimization boundary without copying it.**
   1. Change `trueflow/src/optimizer.rs::optimize` to take `source: &str` alongside `Vec<Block>`.
   2. Pass that borrow through `optimize_imports`, `optimize_modules`, `optimize_code_paragraphs`, every `flush_blocks` call, and `optimize_small_files`. Thresholds, target kinds, buffering decisions, and pass order remain unchanged.
   3. Change `trueflow/src/block_splitter.rs::BlockSplitResult::into_review_blocks` to accept the same `&str`, document that it must be the exact source passed to `split`, and call the source-aware optimizer.
   4. Update every current consumer with its already-available source: `scanner.rs::process_file`, `finder.rs::fuzzy_find_block`, `vcs.rs::split_blocks`, the direct caller in `tests/e2e_elisp.rs`, and optimizer/block-splitter tests. Make a clean signature cutover; do not add an overload, fallback source, inferred source, or compatibility shim.
   5. Run the scanner regressions again. They must remain red until the shared merge implementation changes.

4. **Add unit-level red coverage for one shared source-provenance validator in `trueflow/src/optimizer.rs`.** Use these exact names; all share the `test_source_integrity_` prefix in the focused unit filter.
   1. `test_source_integrity_merge_slices_disjoint_source_and_metadata` uses two eligible blocks whose spans leave omitted whitespace and gives them overlapping tag sets plus explicit complexities. Require exact bounding source content/hash/byte bounds/line bounds, stable first-seen tag union, and saturating complexity sum exactly once.
   2. `test_source_integrity_merge_refuses_unprovable_candidate` covers source-order reversal, pair overlap, `start_byte > end_byte`, an end beyond `source.len()`, a boundary inside a multibyte UTF-8 scalar, and a non-empty block with an empty span. Each case must preserve the whole candidate verbatim and not panic.
   3. `test_source_integrity_flush_refuses_overlapping_whole_buffer` and `test_source_integrity_small_file_refuses_overlapping_whole_vector` prove pass-level whole-candidate refusal; do not merely test the validator in isolation.
   4. `test_source_integrity_code_paragraph_merge_slices_source` protects the third `flush_blocks` user with omitted whitespace, while the scanner regressions protect imports/modules and the Python case protects small files.

5. **Implement a single exact merge primitive and use it everywhere in `trueflow/src/optimizer.rs`.**
   1. Accept `source: &str`, the full participating slice, and the merged kind; return `Option<Block>` or an equivalent result that carries the validated block.
   2. Validate every participating block, including comments and explicit gaps. Require `start_byte <= end_byte`, endpoints within `source.len()`, UTF-8 character boundaries, and a non-empty interval for non-empty content.
   3. Require vector order to be monotonic and pairwise disjoint: for every adjacent pair, `previous.end_byte <= next.start_byte`. Equality is exact adjacency; a positive gap is valid and is restored from source. Any overlap or reversal rejects the candidate.
   4. Derive the bounding interval from the first start through the last end and require `source.get(bound)` to succeed.
   5. Allocate content once from that exact source slice, set issue #18 byte bounds to the same interval, carry the first/last disjoint line bounds, compute the hash from the exact content, and aggregate stable tags/complexity only after disjointness is proven.
   6. Keep this validation/construction in one helper so `flush_blocks` and `optimize_small_files` cannot drift into separate provenance conventions.

6. **Apply the primitive to every merge while preserving each pass's selection policy.**
   1. In `flush_blocks`, retain the current first-target through last-target selection, but remove the synthetic `separator` parameter and all `push_str` assembly. Attempt one exact merge for that entire selected range. If validation fails, return the entire original buffer unchanged—no partial safe-prefix merge, no leading/trailing rearrangement.
   2. Route `optimize_imports`, `optimize_modules`, and `optimize_code_paragraphs` through that source-aware `flush_blocks`. Preserve their existing gap limits, span limits, comments, target counts, kinds, and greedy flush boundaries.
   3. In `optimize_small_files`, preserve the semantic policy currently in `should_merge_small_file`, but make successful eligibility carry the shared validated block/span. Replace its `push_str` loop with the shared exact merge. On failure, preserve the complete original vector.
   4. Preserve `merged_small_file_kind` semantics and `merged_metadata`'s stable union/saturating sum for accepted disjoint input only. Never aggregate parent/child overlap.
   5. Update assertions that previously blessed normalized import/module content: original whitespace is now retained because source exactness is a correctness invariant required by issue #18 parent-relative bytes.

7. **Invalidate all pre-stack scanner output in `trueflow/src/scanner.rs`.**
   1. In the final atomic #18+#19 stack, record the then-current `SCAN_CACHE_FORMAT_VERSION` after issue #17 and increment it exactly once. Baseline commit `9a98914698c4` currently uses 2, but do not hard-code 3: if issue #17 has already raised the constant, issue #19 must advance from that value instead. No intermediate issue-#18-only build may write the final incremented version.
   2. Add red unit test `scan_cache_rejects_pre_source_integrity_format`. Write a structurally valid cache entry whose `format_version` is exactly `SCAN_CACHE_FORMAT_VERSION - 1` after the bump, with matching root/options/file stamp and a stale fabricated optimizer block while the unchanged file on disk contains the correct source. Require `ScanCacheReadStatus::Miss`, zero reused files, one rescanned file, a cache rewritten at the new current format, and source-exact output. This must be a clean version miss, not a deserialization error. The implementation must advance the pre-stack constant by one; the fixture must not assume that either value is 2 or 3.
   3. Keep cache key semantics otherwise unchanged; a version bump is sufficient because the optimizer change affects scanner-visible block content, hashes, spans, and file tree hashes.

8. **Run the focused behavioral smoke before cleanup.** Execute the bug-regression, optimizer-unit, forced-sub-split, and stale-cache commands below. Confirm exact newlines, exact parent/child slices, two independent C# complexities of `Some(1)`, and cache rescan.

9. **Only after the smoke passes, finish mechanical cleanup and validation.** Update existing optimizer test helpers with coherent issue #18 spans and exact source arguments. Remove the obsolete separator/concatenation code and stale normalized-content comments. Preserve unrelated grouping thresholds and splitter gap policy.

### Ordered affected files and symbols

1. `trueflow/tests/bug_regressions.rs`
   - the five exact `test_optimizer_source_integrity_*` regressions above, including the 35-line normal/forced child-provenance fixture.
2. `trueflow/src/optimizer.rs`
   - source-aware `optimize`, `optimize_imports`, `optimize_modules`, `optimize_code_paragraphs`, `flush_blocks`, and `optimize_small_files`;
   - one shared validated exact-source merge primitive;
   - `should_merge_small_file` integration, metadata timing, and all pass-level provenance unit tests.
3. `trueflow/src/block_splitter.rs`
   - `BlockSplitResult::into_review_blocks(&str)` and exact import/module pipeline assertions.
4. `trueflow/src/scanner.rs`
   - `process_file` source argument;
   - one atomic increment from the then-current `SCAN_CACHE_FORMAT_VERSION`;
   - `scan_cache_rejects_pre_source_integrity_format`.
5. `trueflow/src/finder.rs`
   - `fuzzy_find_block` passes loaded content.
6. `trueflow/src/vcs.rs`
   - private `split_blocks` passes its content.
7. `trueflow/tests/e2e_elisp.rs`
   - direct `into_review_blocks` caller passes fixture content without falsifying line/byte provenance.

No new dependency or configuration file is required.

## Verification and validation

Run from the repository root, in this order.

1. Prove all scanner-visible source-integrity regressions are red before the merge fix, then green afterward:

   ```sh
   cd trueflow && cargo test --test bug_regressions test_optimizer_source_integrity_
   ```

2. Prove the shared validator, all four merge paths, metadata, and whole-candidate refusal:

   ```sh
   cd trueflow && cargo test --lib optimizer::tests::test_source_integrity_
   ```

3. Prove the 35-line optimized composite translates both normal and forced child splits onto original file bytes:

   ```sh
   cd trueflow && cargo test --test bug_regressions test_optimizer_source_integrity_preserves_long_module_composite_child_provenance -- --exact
   ```

4. Prove immediately prior-format optimizer output is rejected and rescanned:

   ```sh
   cd trueflow && cargo test --lib scanner::tests::scan_cache_rejects_pre_source_integrity_format -- --exact
   ```

5. Re-run the full optimizer module to protect unchanged grouping thresholds and refusal boundaries:

   ```sh
   cd trueflow && cargo test --lib optimizer::tests
   ```

6. Exercise the source-aware split/optimization boundary:

   ```sh
   cd trueflow && cargo test --lib block_splitter::tests::test_split_includes_optimization_pipeline -- --exact
   ```

7. After the focused smoke and cleanup are complete, run the repository gate:

   ```sh
   just check
   ```

Behavioral checks embedded in the tests must establish all of the following, not merely block counts:

- Python raw structured blocks omit the whitespace-only gap, while the small-file merge restores the original newline.
- Import, module, code-paragraph, and small-file composites are byte-for-byte contiguous source substrings with hashes and spans for that exact content.
- Both normal and forced child-navigation sub-splitting of the specified 35-line `Modules` composite map every child back to the same original source bytes.
- C# parent/child overlap is rejected without panic, partial merge, truncated bounds, or aggregated complexity.
- Invalid, reversed, out-of-range, out-of-order, overlapping, empty, and non-character-boundary spans preserve the entire candidate.
- A cache from the immediately previous format cannot return stale fabricated content after the atomic stack increments the format.

## Acceptance criteria

- Issues #18 and #19 land atomically: required absolute half-open spans exist, every optimizer composite is source-exact, and no intermediate issue-#18-only artifact writes the final cache version.
- Scanning `import os\nx = 1\n` never yields `import osx = 1`. The accepted block is exactly `source[0..15]`, with exact hash, bytes `[0, 15)`, lines `[0, 2)`, tags `[]`, and complexity `Some(0)`.
- Every accepted import, module, code-paragraph, or small-file merge satisfies `merged.content == &source[merged.start_byte..merged.end_byte]`.
- Omitted whitespace between disjoint semantic blocks appears exactly once. The import and module regressions retain their original blank-line bytes rather than normalizing or deleting them.
- Every accepted candidate has valid UTF-8 bounds and monotonically ordered, pairwise-disjoint spans. Every represented source byte and complexity contribution is counted at most once.
- The C# class `[0, 84)` and method `[19, 82)` remain separate with exact content, hashes, bounds, tags, and separate `Some(1)` complexities.
- The exact 35-line/34-declaration `Modules` fixture is above the 32-line normal split threshold and within the 48-line optimizer limit; both normal and forced refinement yield multiple children whose content equals their absolute source slice.
- Invalid bounds, non-UTF-8 boundaries, empty provenance, reversal, out-of-order input, or overlap returns the whole original candidate unchanged and does not panic.
- Accepted disjoint blocks retain stable first-seen tag union and saturating complexity sum; rejected overlap never aggregates metadata.
- `scanner`, `finder`, `vcs`, and direct test callers pass the exact split source through the new API with no shim or inferred reconstruction.
- The atomic stack increments the then-current scan-cache format exactly once; entries from the immediately previous value miss, rescan, and cannot leak stale optimizer content.
- Existing pass ordering, target kinds, count/span thresholds, and greedy grouping boundaries remain unchanged.
- All focused commands and final `just check` pass.

## Non-goals and risks

- Do not change how structured splitters omit whitespace-only gaps. Exact source slicing makes that representation safe.
- Do not remove parent or nested blocks or redesign structured nesting. Overlap remains useful; optimizer merges conservatively refuse it.
- Do not redesign import/module/code-paragraph grouping policy. Their content construction and provenance validation are in scope because issue #18 makes parent-relative offsets observable; thresholds, target selection, and pass order are not.
- Do not change tag detection, complexity scoring, hash canonicalization, `FileState` tree-hash semantics, cache-key inputs beyond the required format bump, or review-tree parenting.
- Do not reconstruct source from line numbers, injected separators, or `content.len()`. Multibyte text and omitted gaps make those unsafe.
- Do not partially merge around invalid provenance. Whole-candidate refusal is the invariant for every pass.
- Source/API risk: `into_review_blocks` must receive the same source passed to `split`. Keep it borrowed and explicit rather than cloning source into `BlockSplitResult`.
- Sub-split risk: a bounding span alone is insufficient if composite content was normalized. Forced child tests must compare each translated absolute child slice with the original file.
- Cache risk: issue #18 and #19 cannot use separate released cache versions while landing atomically. The final stack owns one increment from whatever value is current after issue #17, and no intermediate build may emit the final value.
- Test-fixture risk: synthetic optimizer and forced-split blocks may have incoherent byte spans. Repair their provenance rather than weakening validation or inflating only `end_line`.
- This project is beta. Make a clean atomic cutover with no serialization migration, deprecated overload, or compatibility alias.
